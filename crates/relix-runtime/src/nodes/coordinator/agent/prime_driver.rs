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
    compute_readiness, handle_orchestrate, handle_orchestrate_with_blueprint, handle_prime_approve,
    handle_prime_start, handle_strategy_approve, handle_strategy_propose, handle_team_plan,
    internal, invalid, policy_denied,
};
use crate::nodes::coordinator::agent::prime;
use crate::nodes::coordinator::agent::prime_deliberation::{
    ACTION_NONE, PrimeAiDecider, PrimeDeliberationInput, PrimeDeliberationMode,
    build_prime_deliberation_prompt, parse_prime_decision,
};
use crate::nodes::coordinator::agent::prime_orchestration::{
    PrimeOrchestrationBlueprint, PrimeOrchestrationMode, PrimeOrchestrationRole,
    PrimeOrchestrationSnapshot, build_orchestration_prompt, parse_orchestration_blueprint,
};
use crate::nodes::coordinator::agent::prime_plan_package::{
    PrimePlanPackageMode, PrimePlanPackageSnapshot, PrimePlanPackageTrigger, ValidatedPlanPackage,
    build_plan_package_prompt, deterministic_plan_package, validate_plan_package,
};
use crate::nodes::coordinator::agent::prime_priority::{
    MAX_PRIORITY_CANDIDATES, PrimePriorityCandidate, PrimePriorityMode, build_priority_prompt,
    parse_priority_order,
};
use crate::nodes::coordinator::agent::prime_strategy::{
    PrimeStrategyDraftMode, PrimeStrategyDraftResult, PrimeStrategySnapshot,
    build_strategy_draft_prompt, validate_strategy_draft,
};
use crate::nodes::coordinator::agent::store::{AgentStore, StandingApprovalMatch};
use crate::nodes::coordinator::spine::SpineStore;

/// The one-step advance keys the driver may execute on explicit operator
/// request. Strategy / hire / spawn / budget approvals are deliberately NOT
/// here — they stay human decisions.
const ADVANCE_CREATE_TEAM_PLAN: &str = "create_team_plan";
const ADVANCE_ORCHESTRATE: &str = "orchestrate_assign_ready";
/// Prime Strategy Drafting v1 (company-model §12.5/§12.5A — the Prime planner).
/// The driver may DRAFT a Mandate strategy when none exists and propose it
/// through the existing `mandate.strategy.propose` path. This is **not** strategy
/// approval — the doc lands `proposed` and still needs a human (or an explicit
/// standing grant) to approve before team planning unlocks.
const ADVANCE_PROPOSE_STRATEGY: &str = "propose_strategy";

/// Prime Shift Disposition v1 (company-model §12.6 — the review→apply tail). A
/// completed Shift awaiting review acceptance, or an accepted run awaiting apply,
/// is a real governed next step. These phases are grant-AGNOSTIC in the classifier
/// — [`attemptable_action`] gates the actual autonomous action on the matching
/// SEPARATE standing grant (`prime.run.review_accept` / `prime.run.apply`); with
/// no grant the step stays a human review/apply gate (recorded, never acted on).
const PHASE_NEEDS_REVIEW: &str = "needs_review";
const PHASE_NEEDS_APPLY: &str = "needs_apply";
/// The autonomous disposition actions (distinct from any mandate-advance key).
const ACTION_REVIEW_ACCEPT: &str = "review_accept";
const ACTION_APPLY_RUN: &str = "apply_run";
/// The autonomous plan-package approval action (accept/materialize a
/// Prime-authored plan package through the existing plan-confirm path).
const ACTION_PLAN_PACKAGE_APPROVE: &str = "plan_package_approve";
/// The autonomous Prime-decomposed child assignment action (assign the
/// unassigned children of a Prime-authored decomposition to the parent Brief's
/// own active assignee, through the existing assignee primitive).
const ACTION_ASSIGN_DECOMPOSED: &str = "assign_decomposed_children";

// ── PRIME STANDING AUTHORITY (v1) ──────────────────────────────────────────
// The Board can grant the autonomous Prime loop bounded power to take specific
// governed APPROVAL actions on its behalf — but ONLY through an explicit
// `standing_approvals` row in the tenant, never from env alone. The grant is
// recorded against a SYNTHETIC authority subject (not a real Operative) and one
// of the narrow categories. This is "within powers you granted it", not a
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
/// Standing-authority category: autonomous approval of a PROPOSED Mandate
/// strategy (drives the existing `mandate.strategy.approve` path). Strategy
/// REJECTION stays final — the store only flips `proposed` → `approved`, so a
/// rejected / missing strategy is never approved and never re-proposed here.
pub const CATEGORY_STRATEGY_APPROVE: &str = "prime.strategy.approve";
/// Standing-authority category: autonomous ACCEPTANCE of a COMPLETED Shift's
/// review for a Brief in the candidate Mandate/proposal's OWN Brief set (drives
/// the existing review path — `TaskStore::set_run_review` with `accepted`). Only
/// a `done` + `pending_review` run is ever accepted; acceptance is SEPARATE from
/// apply (the distinct grant below) — a single tick accepts XOR applies one run.
pub const CATEGORY_RUN_REVIEW_ACCEPT: &str = "prime.run.review_accept";
/// Standing-authority category: autonomous APPLY of an already-ACCEPTED run
/// through the EXISTING safe apply machinery (`controller_runtime::execute_run_apply`
/// — `run_apply_eligibility`, baseline/conflict checks, `complete_reviewed_brief`
/// review-to-done). A run that is not `done` + `accepted` + apply-eligible is
/// never applied, and a conflicted/failed apply NEVER marks the Brief done. Apply
/// is SEPARATE from review acceptance (the grant above).
pub const CATEGORY_RUN_APPLY: &str = "prime.run.apply";
/// Standing-authority category: autonomous ACCEPTANCE / materialization of an
/// OPEN plan-package `confirm` that autonomous Prime ITSELF authored (author =
/// [`AUTONOMOUS_PRIME_AUTHORITY`]), through the EXISTING governed plan-confirm
/// path (`TaskStore::respond_plan_confirm`) and the exactly-once decomposition
/// ledger — the SAME primitive a human approval uses, so the behaviour is
/// identical. This is NOT blanket self-approval: it ONLY ever accepts a
/// Prime-authored package (a human/other-actor package is never auto-approved),
/// it never bypasses the ledger or creates children by hand, it is tenant-scoped
/// to a single Brief, and a duplicate / already-materialized package consumes no
/// second grant. With no grant the loop leaves the confirm OPEN exactly as
/// before (the pending package keeps holding a `before_execute` start).
pub const CATEGORY_PLAN_PACKAGE_APPROVE: &str = "prime.plan_package.approve";
/// Standing-authority category: autonomous ASSIGNMENT of the unassigned child
/// Briefs that autonomous Prime's OWN plan-package materialization created
/// (author = [`AUTONOMOUS_PRIME_AUTHORITY`]). Narrow + deterministic: it assigns
/// such a child ONLY to its parent Brief's CURRENT assignee, and ONLY when that
/// assignee is an active, same-Guild Operative with a known Rig — the model
/// never picks an agent. It acts through the EXISTING `set_brief_field`
/// `assignee` primitive (the same one the governed assignment paths use), never
/// scans arbitrary unassigned Briefs, and never touches a human/other-actor
/// decomposition (those keep their human assignment gate). With no grant the
/// children stay unassigned and the
/// loop parks honestly at the assignment gate exactly as before; one bounded
/// grant call is consumed only when at least one child is actually assigned.
pub const CATEGORY_ASSIGN_DECOMPOSED: &str = "prime.brief.assign_decomposed";

/// The eight standing-authority categories, in display order.
pub const STANDING_AUTHORITY_CATEGORIES: &[&str] = &[
    CATEGORY_PROPOSAL_APPROVE,
    CATEGORY_HIRE_APPROVE,
    CATEGORY_CLEARANCE_APPROVE,
    CATEGORY_STRATEGY_APPROVE,
    CATEGORY_RUN_REVIEW_ACCEPT,
    CATEGORY_RUN_APPLY,
    CATEGORY_PLAN_PACKAGE_APPROVE,
    CATEGORY_ASSIGN_DECOMPOSED,
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
    /// The completed run targeted by a `needs_review` / `needs_apply` disposition
    /// step (the deterministic oldest eligible run in the Mandate's Brief set).
    /// `None` for every other phase. Re-validated at execution time.
    run_id: Option<String>,
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
            "run_id": self.run_id,
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

/// Max characters of a Mandate's free-text description folded into a drafted
/// strategy — keeps the proposal bounded regardless of input size. Shared with
/// the model strategy-authoring snapshot in `prime_strategy`.
pub(crate) const STRATEGY_DRAFT_DESC_CAP: usize = 600;
/// Hard cap on the whole drafted strategy body (deterministic or model-authored).
pub(crate) const STRATEGY_DRAFT_BODY_CAP: usize = 4000;

/// Prime Strategy Drafting v1 (company-model §12.5/§12.5A — the Prime planner).
/// Produce a concise, useful strategy doc DETERMINISTICALLY from the Mandate's own
/// fields (title / description / status) plus the known company context (the
/// Guild's active work roles). NO model, NO secret-shaped values — this is the safe
/// deterministic draft Prime proposes when a Mandate has no strategy yet. The
/// result is sanitized for the pipe-delimited `mandate.strategy.propose` wire (the
/// `|` separator is replaced) and length-bounded. It is left `proposed` for human
/// approval: drafting is NOT approval.
pub(crate) fn draft_mandate_strategy(
    mandate: &crate::nodes::coordinator::spine::store::Mandate,
    active_roles: &[&str],
) -> String {
    let title = match mandate.title.trim() {
        "" => "(untitled Mandate)",
        t => t,
    };
    let status = match mandate.status.trim() {
        "" => "planned",
        s => s,
    };
    let desc = mandate.description.trim();
    let objective = if desc.is_empty() {
        format!("Deliver the Mandate \"{title}\".")
    } else if desc.chars().count() > STRATEGY_DRAFT_DESC_CAP {
        let clipped: String = desc.chars().take(STRATEGY_DRAFT_DESC_CAP).collect();
        format!("{clipped}…")
    } else {
        desc.to_string()
    };

    let tracks = if active_roles.is_empty() {
        "No active work crew yet — staff the team (team plan / hires) before execution can begin."
            .to_string()
    } else {
        active_roles
            .iter()
            .map(|r| format!("- {r}: own the {r} work track for this Mandate."))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let body = format!(
        "# Strategy — {title}\n\
         \n\
         Status at drafting: {status}. This is a Prime DRAFT proposal (deterministic v1), not an approved strategy.\n\
         \n\
         ## Objective\n{objective}\n\
         \n\
         ## Constraints\n\
         - Stay within the approved Mandate scope; do not expand beyond \"{title}\".\n\
         - Human approval gates remain in force (strategy, hires, spawn Clearances, budget).\n\
         - Execution runs in scoped Shift workspaces; changes are reviewed before they are applied.\n\
         \n\
         ## Team & work tracks\n{tracks}\n\
         \n\
         ## Execution approach\n\
         - Decompose the Mandate into Briefs and assign each to the relevant work track.\n\
         - Run assigned Briefs as Shifts; keep sub-work and blockers explicit.\n\
         - Sequence dependent work; let independent tracks proceed in parallel.\n\
         \n\
         ## Review & apply policy\n\
         - Every Shift's output is inspected and reviewed before apply.\n\
         - Apply is all-or-nothing per run; conflicts are resolved by re-run, not force.\n\
         \n\
         ## Risks & approvals\n\
         - This strategy is a DRAFT and is NOT approved. A human (or an explicitly granted standing authority) must approve it before team planning and orchestration unlock.\n\
         - Rejecting it stops the work here; Prime will not silently re-propose over a rejection.\n",
    );

    // Sanitize the pipe delimiter and bound the length. The wire is `mandate_id|doc`
    // (splitn(2)), so a stray `|` in the body is harmless in practice, but we replace
    // it defensively to keep the pipe contract unambiguous.
    let mut body = body.replace('|', "/");
    if body.chars().count() > STRATEGY_DRAFT_BODY_CAP {
        body = body.chars().take(STRATEGY_DRAFT_BODY_CAP).collect();
    }
    body
}

/// Prime Strategy Authoring v1 (company-model §12.5/§12.5A — the Prime planner).
/// Author the strategy DRAFT body to propose for a Mandate. When model authoring
/// is enabled (`strategy_llm_enabled`) AND a live decider is wired, the model
/// authors the body from the SAME safe, bounded snapshot the deterministic draft
/// uses (title / status / bounded description / active roles / readiness counts);
/// the reply is fully re-validated + sanitized + governance-footered by
/// [`validate_strategy_draft`]. **This is NOT approval:** the returned doc is
/// always proposed through the existing `mandate.strategy.propose` handler and
/// lands `proposed`. If model authoring is off, no decider is wired, the model is
/// unavailable, or its output is rejected, the body degrades to the deterministic
/// [`draft_mandate_strategy`] with an honest [`PrimeStrategyDraftMode`]. The
/// returned `doc` is always non-empty, pipe-safe, and `STRATEGY_DRAFT_BODY_CAP`-
/// bounded.
fn draft_strategy_doc(
    ai: Option<&dyn PrimeAiDecider>,
    mandate: &crate::nodes::coordinator::spine::store::Mandate,
    active_roles: &[&str],
    counts: Option<&BriefCounts>,
    strategy_llm_enabled: bool,
) -> PrimeStrategyDraftResult {
    let deterministic = || draft_mandate_strategy(mandate, active_roles);
    if !strategy_llm_enabled {
        return PrimeStrategyDraftResult {
            doc: deterministic(),
            mode: PrimeStrategyDraftMode::DeterministicOnly,
            reason: None,
        };
    }
    let Some(decider) = ai else {
        return PrimeStrategyDraftResult {
            doc: deterministic(),
            mode: PrimeStrategyDraftMode::Unavailable,
            reason: Some("no AI decider wired for strategy drafting".to_string()),
        };
    };
    let snap = PrimeStrategySnapshot::new(
        &mandate.title,
        &mandate.status,
        &mandate.description,
        active_roles,
        counts.map(|c| (c.total, c.ready, c.running)),
    );
    let prompt = build_strategy_draft_prompt(&snap);
    match decider.deliberate(&prompt) {
        Ok(raw) => match validate_strategy_draft(&raw) {
            Ok(doc) => PrimeStrategyDraftResult {
                doc,
                mode: PrimeStrategyDraftMode::LlmUsed,
                reason: Some("model-authored strategy draft".to_string()),
            },
            Err(e) => PrimeStrategyDraftResult {
                doc: deterministic(),
                mode: PrimeStrategyDraftMode::Fallback,
                reason: Some(format!("model strategy output rejected: {e}")),
            },
        },
        Err(e) => PrimeStrategyDraftResult {
            doc: deterministic(),
            mode: PrimeStrategyDraftMode::Unavailable,
            reason: Some(format!("model unavailable: {e}")),
        },
    }
}

/// The `max_briefs` cap the autonomous/manual Prime tick orchestrates under — the
/// SAME default the deterministic `mandate.orchestrate` uses when the operator
/// passes no cap (the tick dispatches `{mandate_id}|assign_ready`). Used only to
/// bound the authoring snapshot fed to the model; the handler re-derives + clamps
/// the real cap itself.
const AUTONOMOUS_ORCH_MAX_BRIEFS: usize = 16;

/// Build the bounded, secret-free orchestration-authoring snapshot for a Mandate
/// — the SAME active/gap role computation `handle_orchestrate` performs, so the
/// offered role / subject keys match exactly what the handler will materialise.
/// Returns `None` (→ deterministic fallback) when the Mandate or its readiness is
/// unavailable. NO secret, credential, token, or repo content is included; only
/// the Mandate's own title/status, a bounded approved-strategy excerpt, the active
/// role keys + their staffed agent ids, and the gap roles + reasons (context only).
fn build_orchestration_snapshot(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    tenant: &str,
    mandate_id: &str,
) -> Option<PrimeOrchestrationSnapshot> {
    let mandate = spine_store
        .get_mandate_for_tenant(mandate_id, tenant)
        .ok()??;
    let view = compute_readiness(agent_store, spine_store, tenant, mandate_id).ok()?;

    // Active role tracks — deduped by lowercased role key, first staffed agent
    // wins (mirrors the handler's `active` BTreeMap).
    let mut active: std::collections::BTreeMap<String, PrimeOrchestrationRole> =
        std::collections::BTreeMap::new();
    for (role, agent_id) in &view.active_agents {
        let key = role.trim().to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        active
            .entry(key.clone())
            .or_insert_with(|| PrimeOrchestrationRole {
                role_key: key.clone(),
                agent_id: agent_id.clone(),
            });
    }
    let active_keys: std::collections::BTreeSet<String> = active.keys().cloned().collect();

    // Gap roles (context only) — missing / pending / blocked; an active role is
    // never a gap. First writer wins (most specific reason first), mirroring the
    // handler's `gap` BTreeMap ordering.
    let mut gap: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    {
        let mut note_gap = |role: &str, reason: &str| {
            let key = role.trim().to_ascii_lowercase();
            if key.is_empty() || active_keys.contains(&key) {
                return;
            }
            gap.entry(key).or_insert_with(|| reason.to_string());
        };
        for c in &view.pending_clearances {
            if let Some(role) = c.get("role").and_then(|v| v.as_str()) {
                note_gap(role, "pending clearance");
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

    // Only feed the model an APPROVED strategy excerpt (the orchestrate gate
    // requires an approved strategy; a non-approved doc is never authored from).
    let strategy = match spine_store.strategy_approved(tenant, mandate_id) {
        Ok(true) => spine_store.strategy_doc(tenant, mandate_id).ok().flatten(),
        _ => None,
    };

    Some(PrimeOrchestrationSnapshot::new(
        &mandate.title,
        &mandate.status,
        strategy.as_deref(),
        active.into_values().collect(),
        gap.into_iter().collect(),
        AUTONOMOUS_ORCH_MAX_BRIEFS,
    ))
}

/// Prime Orchestration Authoring v1 (company-model §4.6 / §12.5A). Author an
/// orchestration TEXT blueprint for the Mandate's already-computed skeleton when
/// model authoring is enabled (`orchestration_llm_enabled`) AND a live decider is
/// wired; the reply is fully re-validated + key-constrained + sanitized by
/// [`parse_orchestration_blueprint`]. **The model authors text only:** the
/// returned blueprint can change only the title / dossier / checklist of
/// newly-created parent / role-track / subject Briefs — never a role, agent, id,
/// assignment, dependency, or gate (all fixed by `handle_orchestrate`). If
/// authoring is off, no decider is wired, the snapshot is unavailable, the model
/// is unreachable, or its output is rejected, this returns `None` with an honest
/// [`PrimeOrchestrationMode`] and the caller falls back to the deterministic text.
fn author_orchestration_blueprint(
    ai: Option<&dyn PrimeAiDecider>,
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    tenant: &str,
    mandate_id: &str,
    orchestration_llm_enabled: bool,
) -> (
    Option<PrimeOrchestrationBlueprint>,
    PrimeOrchestrationMode,
    Option<String>,
) {
    if !orchestration_llm_enabled {
        return (None, PrimeOrchestrationMode::DeterministicOnly, None);
    }
    let Some(decider) = ai else {
        return (
            None,
            PrimeOrchestrationMode::Unavailable,
            Some("no AI decider wired for orchestration authoring".to_string()),
        );
    };
    let Some(snap) = build_orchestration_snapshot(agent_store, spine_store, tenant, mandate_id)
    else {
        return (
            None,
            PrimeOrchestrationMode::Fallback,
            Some("orchestration snapshot unavailable".to_string()),
        );
    };
    let role_keys = snap.offered_role_keys();
    let subject_keys = snap.offered_subject_keys();
    let prompt = build_orchestration_prompt(&snap);
    match decider.deliberate(&prompt) {
        Ok(raw) => match parse_orchestration_blueprint(&raw, &role_keys, &subject_keys) {
            Ok(bp) => (
                Some(bp),
                PrimeOrchestrationMode::LlmUsed,
                Some("model-authored orchestration blueprint".to_string()),
            ),
            Err(e) => (
                None,
                PrimeOrchestrationMode::Fallback,
                Some(format!("model orchestration output rejected: {e}")),
            ),
        },
        Err(e) => (
            None,
            PrimeOrchestrationMode::Unavailable,
            Some(format!("model unavailable: {e}")),
        ),
    }
}

// ── PRIME PLAN-PACKAGE AUTHORING v1 (opt-in) ────────────────────────────────
// A bounded, additive planner step for the autonomous/manual Prime tick: for a
// candidate the existing flow leaves idle, author a *proposed* decomposition plan
// package (plan Dossier + suggest_tasks proposal + approval-bound confirm) on a
// single un-decomposed Brief and leave the confirm OPEN for a human. Reuses the
// EXISTING `open_plan_package` primitive — no duplicate Dossier/interaction logic.

/// Outcome of checking whether an autonomous-tick candidate has a Brief that
/// needs a *proposed* decomposition plan (Prime Plan-Package Authoring v1).
enum PlanPackageEligibility {
    /// The candidate's SOLE Brief is a non-terminal, childless leaf with no `plan`
    /// Dossier, no `plan` lock, and no open `suggest_tasks` / plan `confirm` — Prime
    /// may author a plan package on it. Carries `(brief_id, brief_title, brief_status)`.
    Eligible {
        brief_id: String,
        brief_title: String,
        brief_status: String,
    },
    /// A plan package (open `suggest_tasks` / plan `confirm`), an existing `plan`
    /// Dossier, or a `plan` lock already covers the Brief — skip without authoring a
    /// duplicate or clobbering a human/locked plan. Carries an honest reason and
    /// whether a plan package is PENDING approval right now: `pending_package = true`
    /// only for an open `suggest_tasks`/plan `confirm` (a decomposition a human must
    /// still decide), `false` for a mere existing/locked plan Dossier. The active
    /// planner (`before_execute`) HOLDS the raw start while a package is pending, but
    /// lets an already-planned/locked Brief proceed to start; the idle tail reports
    /// either case honestly.
    Blocked {
        reason: String,
        pending_package: bool,
    },
    /// The candidate has no single un-decomposed Brief to plan — fall through to the
    /// existing human-gate tail (no plan package this tick).
    NotApplicable,
}

/// Decide whether the candidate Mandate has exactly one un-decomposed Brief that
/// warrants a proposed decomposition plan package. Conservative + tenant-scoped:
/// it acts ONLY when the Mandate has a SINGLE Brief (an orchestrated Mandate has
/// many Briefs and is left to the existing flow — this never floods the board), and
/// that Brief is non-terminal (`done`/`cancelled` are never decomposed), childless
/// (no Sub-briefs), has NO `plan` Dossier at all (so a human-, Prime-, or
/// stale-authored plan is NEVER overwritten), is not `plan`-locked, and has no open
/// plan package already awaiting approval. All reads are tenant-scoped: a Brief not
/// in `tenant` is invisible. Any store error degrades to `NotApplicable` (safe — no
/// authoring on an unreadable candidate).
fn plan_package_eligibility(
    task_store: &TaskStore,
    tenant: &str,
    mandate_id: &str,
) -> PlanPackageEligibility {
    let Ok(briefs) = task_store.list_briefs_by_mandate(mandate_id, 500) else {
        return PlanPackageEligibility::NotApplicable;
    };
    // Only a single un-decomposed Brief is a plan-package candidate (a many-Brief
    // Mandate is already planned/orchestrated — leave it to the existing flow).
    if briefs.len() != 1 {
        return PlanPackageEligibility::NotApplicable;
    }
    let card = &briefs[0];
    let brief_id = card.task_id.clone();
    // Tenant isolation: never read/write a Brief outside this Guild.
    if !task_store
        .task_in_tenant(&brief_id, tenant)
        .unwrap_or(false)
    {
        return PlanPackageEligibility::NotApplicable;
    }
    // Terminal Briefs (done/cancelled) are never decomposed.
    if matches!(card.board_status.as_str(), "done" | "cancelled") {
        return PlanPackageEligibility::NotApplicable;
    }
    // Already decomposed (has Sub-briefs) — nothing to plan.
    match task_store.list_subbriefs(&brief_id) {
        Ok(kids) if !kids.is_empty() => return PlanPackageEligibility::NotApplicable,
        Err(_) => return PlanPackageEligibility::NotApplicable,
        Ok(_) => {}
    }
    // An open plan package (suggest_tasks proposal or plan-bound confirm) already
    // awaits approval — never open a duplicate, and this is a PENDING decomposition.
    // Checked BEFORE the plan-Dossier guard so our OWN just-opened package (which
    // also writes a plan Dossier) is recognised as `pending_package`, not merely
    // "a plan Dossier exists" — the active planner must HOLD the start while it is
    // pending, not let the raw Brief slip past on the next tick.
    match task_store.list_interactions(&brief_id) {
        Ok(ix) => {
            let open_suggestion = ix
                .iter()
                .any(|i| i.kind == "suggest_tasks" && i.status == "open");
            let open_plan_confirm = ix.iter().any(|i| {
                i.kind == "confirm"
                    && i.status == "open"
                    && i.bound_doc_kind.as_deref() == Some("plan")
            });
            if open_suggestion || open_plan_confirm {
                return PlanPackageEligibility::Blocked {
                    reason: "an open plan package already awaits approval on the Brief".to_string(),
                    pending_package: true,
                };
            }
        }
        Err(_) => return PlanPackageEligibility::NotApplicable,
    }
    // A `plan` Dossier already exists — never overwrite a human/Prime/stale plan.
    // NOT a pending package (no open confirm), so a preemptive caller may still let
    // the already-planned Brief proceed to its normal start.
    match task_store.latest_dossier(&brief_id, "plan") {
        Ok(Some(_)) => {
            return PlanPackageEligibility::Blocked {
                reason: "a plan Dossier already exists on the Brief — not overwriting".to_string(),
                pending_package: false,
            };
        }
        Err(_) => return PlanPackageEligibility::NotApplicable,
        Ok(None) => {}
    }
    // The `plan` Dossier is locked by someone — respect the lease, do not author.
    // Also not a pending package (someone is actively planning, not awaiting approval).
    match task_store.list_dossier_locks(&brief_id) {
        Ok(locks) if locks.iter().any(|l| l.kind == "plan") => {
            return PlanPackageEligibility::Blocked {
                reason: "the Brief's plan Dossier is locked — not authoring over a held lock"
                    .to_string(),
                pending_package: false,
            };
        }
        Err(_) => return PlanPackageEligibility::NotApplicable,
        Ok(_) => {}
    }
    PlanPackageEligibility::Eligible {
        brief_id,
        brief_title: card.title.clone(),
        brief_status: card.board_status.clone(),
    }
}

/// Author the plan-package CONTENT (plan title/body, summary, child Briefs) for an
/// eligible Brief. When model authoring is enabled (`plan_package_llm_enabled`) AND
/// a live decider is wired, the model authors the content from the SAME bounded,
/// secret-free snapshot; the reply is fully re-validated + sanitized by
/// [`validate_plan_package`]. **This is NOT approval:** the caller opens the content
/// through `open_plan_package` and leaves the confirm OPEN. On disabled / no decider
/// / unavailable / rejected output the content degrades to the deterministic
/// [`deterministic_plan_package`] with an honest [`PrimePlanPackageMode`]. The model
/// authors content only — it never assigns agents (children open unassigned), picks
/// tools/methods, or approves anything.
fn author_plan_package_content(
    ai: Option<&dyn PrimeAiDecider>,
    brief_title: &str,
    brief_status: &str,
    mandate_title: &str,
    plan_package_llm_enabled: bool,
) -> (ValidatedPlanPackage, PrimePlanPackageMode, Option<String>) {
    let deterministic = || deterministic_plan_package(brief_title);
    if !plan_package_llm_enabled {
        return (
            deterministic(),
            PrimePlanPackageMode::DeterministicOnly,
            None,
        );
    }
    let Some(decider) = ai else {
        return (
            deterministic(),
            PrimePlanPackageMode::Unavailable,
            Some("no AI decider wired for plan-package authoring".to_string()),
        );
    };
    let snap = PrimePlanPackageSnapshot::new(brief_title, brief_status, mandate_title);
    let prompt = build_plan_package_prompt(&snap);
    match decider.deliberate(&prompt) {
        Ok(raw) => match validate_plan_package(&raw, brief_title) {
            Ok(vp) => (
                vp,
                PrimePlanPackageMode::LlmUsed,
                Some("model-authored plan package".to_string()),
            ),
            Err(e) => (
                deterministic(),
                PrimePlanPackageMode::Fallback,
                Some(format!("model plan-package output rejected: {e}")),
            ),
        },
        Err(e) => (
            deterministic(),
            PrimePlanPackageMode::Unavailable,
            Some(format!("model unavailable: {e}")),
        ),
    }
}

/// Try to author a Prime plan package for `mandate_id`'s lone eligible un-decomposed
/// Brief, returning `Some(record)` when the plan-package step HANDLED the candidate
/// (authored a package, or honestly reported a budget/block/refusal) — the caller
/// returns that record verbatim — and `None` when no single un-decomposed Brief is
/// eligible (the caller proceeds with its normal flow). This is the SHARED open path
/// for BOTH the active-planner preemption (v2 `before_execute`, called before a raw
/// start) and the idle-tail gap-fill (v1 `tail`): identical eligibility, identical
/// `open_plan_package` primitive, identical author/validate/fallback content — only
/// the call SITE differs. It NEVER self-approves, assigns agents, picks tools, or
/// creates children: the bound confirm is left OPEN for a human. Bounded by the
/// action budget (`actions`/`max`), tenant-scoped, and idempotent / non-clobbering
/// via [`plan_package_eligibility`]. `mk` builds the candidate's base record; this
/// stamps the plan-package provenance + ids + the effective `trigger` on it.
///
/// `preempting` controls what happens when a plan package can't be authored but the
/// Brief is already covered: the active-planner caller (`true`, the (B0) step before
/// a raw start) HOLDS the start ONLY while a package is PENDING approval, and lets an
/// already-planned/locked Brief fall through to its normal start (returns `None`);
/// the idle-tail caller (`false`) reports either block honestly (returns `Some`), as
/// it sits at the end of the pipeline anyway.
#[allow(clippy::too_many_arguments)]
fn try_open_plan_package_for_mandate(
    task_store: &TaskStore,
    spine_store: &SpineStore,
    ai: Option<&dyn PrimeAiDecider>,
    tenant: &str,
    mandate_id: &str,
    plan_package_llm_enabled: bool,
    trigger: PrimePlanPackageTrigger,
    preempting: bool,
    actions: &mut usize,
    max: usize,
    phase: &str,
    mk: &dyn Fn(String, &'static str, &'static str, String, Option<String>) -> PrimeAutonomyRecord,
) -> Option<PrimeAutonomyRecord> {
    let trig = trigger.as_str().to_string();
    match plan_package_eligibility(task_store, tenant, mandate_id) {
        PlanPackageEligibility::Eligible {
            brief_id,
            brief_title,
            brief_status,
        } => {
            if *actions >= max {
                let mut rec = mk(
                    phase.to_string(),
                    "plan_package",
                    "skipped",
                    "tick action budget reached".into(),
                    Some(mandate_id.to_string()),
                );
                rec.plan_package_trigger = Some(trig);
                return Some(rec);
            }
            // The owning Mandate title is context for the model snapshot only
            // (best-effort; absence does not block authoring).
            let mandate_title = spine_store
                .get_mandate_for_tenant(mandate_id, tenant)
                .ok()
                .flatten()
                .map(|m| m.title)
                .unwrap_or_default();
            let (vp, pp_mode, pp_reason) = author_plan_package_content(
                ai,
                &brief_title,
                &brief_status,
                &mandate_title,
                plan_package_llm_enabled,
            );
            let child_count = vp.children.len();
            let rec = match task_store.open_plan_package(
                &brief_id,
                AUTONOMOUS_PRIME_AUTHORITY,
                &vp.plan_title,
                &vp.plan_body,
                &vp.summary,
                &vp.children,
                &vp.prompt,
            ) {
                Ok(pkg) => {
                    *actions += 1;
                    // Chronicle ids/counts/mode only — never the plan body.
                    chronicle_autonomous(
                        task_store,
                        mandate_id,
                        "prime.autonomous_plan_package",
                        &format!(
                            "autonomous Prime opened a plan package on brief {brief_id} ({trig}): plan {} + {child_count} child(ren), gated by confirm {} (content: {})",
                            pkg.plan_doc_id,
                            pkg.confirm_id,
                            pp_mode.as_str()
                        ),
                    );
                    let mut rec = mk(
                        phase.to_string(),
                        "plan_package",
                        "advanced",
                        format!(
                            "opened a plan package on brief {brief_id} ({child_count} proposed child(ren)); confirm {} left open for approval",
                            pkg.confirm_id
                        ),
                        Some(mandate_id.to_string()),
                    );
                    rec.plan_package_ai_mode = Some(pp_mode.as_str().to_string());
                    rec.plan_package_ai_reason = pp_reason;
                    rec.plan_doc_id = Some(pkg.plan_doc_id);
                    rec.suggestion_id = Some(pkg.suggestion_id);
                    rec.confirm_id = Some(pkg.confirm_id);
                    rec.child_count = Some(child_count);
                    rec
                }
                // Store refusal (e.g. a race created a Brief edge / plan since the
                // eligibility read) — propagate honestly, take no credit.
                Err(e) => mk(
                    phase.to_string(),
                    "plan_package",
                    "blocked",
                    format!("plan package open refused: {e}"),
                    Some(mandate_id.to_string()),
                ),
            };
            let mut rec = rec;
            rec.plan_package_trigger = Some(trig);
            Some(rec)
        }
        // A plan package / plan Dossier / lock already covers the Brief — author no
        // duplicate. When PREEMPTING a raw start and the block is a mere existing /
        // locked plan (NOT a pending package), return `None` so the already-planned
        // Brief proceeds to its normal start instead of stalling forever; a PENDING
        // package always holds (the human must decide the proposed decomposition
        // first). The idle tail reports either case honestly.
        PlanPackageEligibility::Blocked {
            reason,
            pending_package,
        } => {
            if preempting && !pending_package {
                return None;
            }
            let mut rec = mk(
                phase.to_string(),
                "plan_package",
                "skipped",
                reason,
                Some(mandate_id.to_string()),
            );
            rec.plan_package_trigger = Some(trig);
            Some(rec)
        }
        // No single un-decomposed Brief — the caller proceeds with its normal flow.
        PlanPackageEligibility::NotApplicable => None,
    }
}

// ── PRIME PLAN-PACKAGE APPROVAL — STANDING AUTHORITY v1 ─────────────────────
// The next safe slice of autonomy: let the loop ACCEPT/materialize a plan
// package it ITSELF opened, but ONLY when the Board has granted the explicit
// `prime.plan_package.approve` standing authority for the Guild. It is NOT
// blanket self-approval — it accepts a Prime-authored package only, through the
// EXISTING governed plan-confirm path (`respond_plan_confirm`) and the
// exactly-once decomposition ledger (no hand-rolled child creation, no ledger
// bypass), and consumes one bounded grant call only on a real materialization.

/// Try to ACCEPT an OPEN plan-package confirm that autonomous Prime itself
/// authored on `mandate_id`'s lone Brief, returning `Some(record)` when the
/// approval step HANDLED the candidate (materialized the package, or honestly
/// reported a budget/refusal) — the caller returns that record verbatim — and
/// `None` when there is nothing to approve OR no standing grant (the caller
/// proceeds with its normal flow: the pending package keeps holding the start /
/// is reported by the open path). Runs BEFORE opening a duplicate package and
/// BEFORE a raw start, so a pending Prime-authored package is an actionable
/// governance gate the moment the grant exists.
///
/// Strictly bounded:
/// - **Prime-authored only** — the confirm's `author` must be
///   [`AUTONOMOUS_PRIME_AUTHORITY`]; a human/other-actor package is never
///   auto-approved (returns `None`).
/// - **Single-Brief, tenant-scoped** — same conservative scope as the open path;
///   a many-Brief Mandate or a cross-Guild Brief is invisible (returns `None`).
/// - **Grant-gated** — with no live `prime.plan_package.approve` standing grant
///   in `tenant` it takes NO side effect and consumes NO grant (returns `None`),
///   leaving the confirm OPEN exactly as before.
/// - **Existing path + ledger** — acceptance flows through
///   [`TaskStore::respond_plan_confirm`] (identical to the human approval), so
///   the exactly-once ledger materializes the children; children always open
///   unassigned (an autonomous package sets no assignee hints), so an empty
///   resolved-assignee slice is correct.
/// - **Idempotent** — once accepted the confirm is `resolved`, so a re-tick finds
///   no OPEN Prime confirm and neither duplicates children nor consumes a second
///   grant. A store refusal (e.g. a stale plan) is reported honestly with no
///   grant consumed.
#[allow(clippy::too_many_arguments)]
fn try_approve_prime_plan_package_for_mandate(
    agent_store: &AgentStore,
    task_store: &TaskStore,
    tenant: &str,
    mandate_id: &str,
    now_ms: i64,
    actions: &mut usize,
    max: usize,
    phase: &str,
    mk: &dyn Fn(String, &'static str, &'static str, String, Option<String>) -> PrimeAutonomyRecord,
) -> Option<PrimeAutonomyRecord> {
    // Same conservative scope as the open path: act only on a single-Brief
    // candidate Mandate, and never on a Brief outside this Guild.
    let briefs = task_store.list_briefs_by_mandate(mandate_id, 500).ok()?;
    if briefs.len() != 1 {
        return None;
    }
    let brief_id = briefs[0].task_id.clone();
    if !task_store
        .task_in_tenant(&brief_id, tenant)
        .unwrap_or(false)
    {
        return None;
    }
    // Find the OPEN plan-package confirm Prime itself authored: a `confirm` bound
    // to a `plan` Dossier WITH a linked `suggest_tasks` proposal, authored by the
    // synthetic autonomous-Prime authority. A human/other-actor package (any other
    // author) is deliberately invisible here — this authority is Prime-authored
    // packages only.
    let ix = task_store.list_interactions(&brief_id).ok()?;
    let confirm = ix.iter().find(|i| {
        i.kind == "confirm"
            && i.status == "open"
            && i.bound_doc_kind.as_deref() == Some("plan")
            && i.bound_interaction_id
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
            && i.author == AUTONOMOUS_PRIME_AUTHORITY
    })?;

    // A Prime-authored package EXISTS. Whether we accept it depends solely on the
    // standing grant: with none, leave the confirm OPEN (no side effect, no grant
    // consumed) and let the caller's normal flow hold/report it.
    let now_secs = now_secs_from_ms(now_ms);
    if !standing_active(agent_store, tenant, CATEGORY_PLAN_PACKAGE_APPROVE, now_secs) {
        return None;
    }
    if *actions >= max {
        return Some(mk(
            phase.to_string(),
            ACTION_PLAN_PACKAGE_APPROVE,
            "skipped",
            "tick action budget reached".into(),
            Some(mandate_id.to_string()),
        ));
    }

    // Accept through the EXISTING governed plan-confirm path + exactly-once
    // decomposition ledger — the same primitive the human approval uses. Children
    // open unassigned (no assignee hints on an autonomous package), so an empty
    // resolved-assignee slice is correct; the ledger no-ops over any extra slot.
    let confirm_id = confirm.interaction_id.clone();
    match task_store.respond_plan_confirm(
        tenant,
        AUTONOMOUS_PRIME_AUTHORITY,
        &brief_id,
        &confirm_id,
        AUTONOMOUS_PRIME_AUTHORITY,
        true,
        &[],
    ) {
        Ok(result) => {
            *actions += 1;
            // Consume ONE bounded grant call ONLY on a real materialization
            // (`approved`). An idempotent `already_approved` (the ledger had
            // already materialized) takes no second grant.
            if result.outcome == "approved" {
                let _ =
                    consume_standing(agent_store, tenant, CATEGORY_PLAN_PACKAGE_APPROVE, now_secs);
            }
            chronicle_autonomous(
                task_store,
                mandate_id,
                "prime.autonomous_plan_package_approve",
                &format!(
                    "autonomous Prime approved its plan package on brief {brief_id} (confirm {confirm_id}, {}): materialized {} child Brief(s)",
                    result.outcome,
                    result.created.len()
                ),
            );
            let mut rec = mk(
                phase.to_string(),
                ACTION_PLAN_PACKAGE_APPROVE,
                "advanced",
                format!(
                    "approved Prime-authored plan package on brief {brief_id} ({}); materialized {} child Brief(s)",
                    result.outcome,
                    result.created.len()
                ),
                Some(mandate_id.to_string()),
            );
            rec.suggestion_id = Some(result.suggestion_id);
            rec.confirm_id = Some(confirm_id);
            rec.child_count = Some(result.created.len());
            Some(rec)
        }
        // Store refusal (e.g. the plan went stale, or a race) — report honestly,
        // take no credit, consume no grant; the confirm is left as the store left it.
        Err(e) => Some(mk(
            phase.to_string(),
            ACTION_PLAN_PACKAGE_APPROVE,
            "blocked",
            format!("plan package approve refused: {e}"),
            Some(mandate_id.to_string()),
        )),
    }
}

// The next safe slice of autonomy after self-approval: let the loop ASSIGN the
// unassigned children that Prime's OWN plan-package materialization created, but
// ONLY when the Board has granted the explicit `prime.brief.assign_decomposed`
// standing authority for the Guild, and ONLY to the parent Brief's own active
// assignee. It is NOT a free assignment engine — it never scans arbitrary
// unassigned Briefs, never lets the model pick an agent, and never touches a
// human/other-actor decomposition. This closes the last E2E caveat ("no
// autonomous assignment of Prime-decomposed children") in the common safe case.

/// Try to ASSIGN the unassigned child Briefs that autonomous Prime's OWN
/// plan-package materialization created under `mandate_id`, returning
/// `Some(record)` when this step HANDLED the candidate (assigned ≥1 child, or
/// honestly reported a budget / no-safe-assignee block) — the caller returns it
/// verbatim — and `None` when there is nothing to assign OR no standing grant
/// (the caller proceeds with its normal flow: orchestration parks honestly at
/// the assignment gate exactly as before). Runs BEFORE the orchestration advance
/// (which cannot adopt Prime-decomposed children and only no-ops), so a freshly
/// materialized Prime-decomposed child set is assigned the moment the grant
/// exists and subsequent ticks can start the child Shifts.
///
/// Strictly bounded — the narrow deterministic rule:
/// - **Prime-decomposed only** — acts ONLY on the unassigned Sub-briefs of a
///   parent Brief whose plan-package `confirm` was authored by
///   [`AUTONOMOUS_PRIME_AUTHORITY`] AND is `resolved` (Prime opened the package
///   AND its decomposition materialized). A human/other-actor decomposition has
///   a different `author` and is invisible here (returns `None`).
/// - **Parent's own assignee only** — children are assigned to the parent
///   Brief's CURRENT assignee, NEVER a model-picked agent, and ONLY when that
///   assignee is an active, same-Guild Operative with a known Rig (the shape the
///   run path expects). No parent assignee / inactive / unknown-Rig / cross-Guild
///   assignee → no assignment, an honest `blocked` record, no grant consumed.
/// - **Grant-gated** — with no live `prime.brief.assign_decomposed` standing
///   grant in `tenant` it takes NO side effect and consumes NO grant (`None`),
///   leaving the children unassigned exactly as before.
/// - **Existing primitive** — assignment flows through
///   [`TaskStore::set_brief_field`] `assignee` (the same primitive the governed
///   assignment paths use), which resets any stale Claim and Chronicles the
///   assignment; no hand-rolled bypass of a permission-sensitive invariant.
/// - **Bounded + idempotent** — ONE tick action + ONE bounded grant call are
///   consumed only when ≥1 child is actually assigned (a batch counts once). Once
///   assigned the children leave the unassigned set, so a re-tick finds none and
///   neither reassigns nor consumes a second grant.
#[allow(clippy::too_many_arguments)]
fn try_assign_decomposed_children_for_mandate(
    agent_store: &AgentStore,
    task_store: &TaskStore,
    tenant: &str,
    mandate_id: &str,
    now_ms: i64,
    actions: &mut usize,
    max: usize,
    phase: &str,
    mk: &dyn Fn(String, &'static str, &'static str, String, Option<String>) -> PrimeAutonomyRecord,
) -> Option<PrimeAutonomyRecord> {
    // Find the parent Brief whose plan package autonomous Prime ITSELF authored
    // AND materialized: a `resolved` `confirm` bound to a `plan` Dossier WITH a
    // linked proposal, authored by the synthetic autonomous-Prime authority. A
    // human/other-actor decomposition (any other author) is deliberately invisible
    // — this authority touches Prime-decomposed children only.
    let briefs = task_store.list_briefs_by_mandate(mandate_id, 500).ok()?;
    let mut parent_id: Option<String> = None;
    for b in &briefs {
        if !task_store
            .task_in_tenant(&b.task_id, tenant)
            .unwrap_or(false)
        {
            continue;
        }
        let ix = task_store.list_interactions(&b.task_id).ok()?;
        let prime_authored = ix.iter().any(|i| {
            i.kind == "confirm"
                && i.status == "resolved"
                && i.bound_doc_kind.as_deref() == Some("plan")
                && i.bound_interaction_id
                    .as_deref()
                    .is_some_and(|s| !s.trim().is_empty())
                && i.author == AUTONOMOUS_PRIME_AUTHORITY
        });
        if prime_authored {
            parent_id = Some(b.task_id.clone());
            break;
        }
    }
    let parent_id = parent_id?;

    // The UNASSIGNED Sub-briefs of that Prime-decomposed parent (same Guild, no
    // assignee). None unassigned → nothing to do (an idempotent re-tick).
    let kids = task_store.list_subbriefs(&parent_id).ok()?;
    let mut unassigned: Vec<String> = Vec::new();
    for k in &kids {
        if !task_store.task_in_tenant(k, tenant).unwrap_or(false) {
            continue;
        }
        let has_assignee = task_store
            .brief_card(k)
            .ok()
            .flatten()
            .and_then(|c| c.assignee_agent_id)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_assignee {
            unassigned.push(k.clone());
        }
    }
    if unassigned.is_empty() {
        return None;
    }

    // A Prime-decomposed child set awaits assignment. Whether we act depends
    // SOLELY on the standing grant: with none, take NO side effect and let the
    // caller's normal flow park honestly at the assignment gate.
    let now_secs = now_secs_from_ms(now_ms);
    if !standing_active(agent_store, tenant, CATEGORY_ASSIGN_DECOMPOSED, now_secs) {
        return None;
    }

    // The parent's CURRENT assignee — the ONLY agent these children may inherit.
    // The model never picks; an absent assignee blocks honestly.
    let parent_assignee = task_store
        .brief_card(&parent_id)
        .ok()
        .flatten()
        .and_then(|c| c.assignee_agent_id)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(assignee) = parent_assignee else {
        return Some(mk(
            phase.to_string(),
            ACTION_ASSIGN_DECOMPOSED,
            "blocked",
            format!(
                "Prime-decomposed children on brief {parent_id} await assignment but the parent \
                 Brief has no assignee to inherit — leaving them unassigned"
            ),
            Some(mandate_id.to_string()),
        ));
    };
    // The parent assignee must be an ACTIVE, same-Guild Operative with a known
    // Rig (the same shape the run path expects). Otherwise block honestly — never
    // assign work to an inactive / un-rigged / cross-Guild subject.
    let runnable = agent_store
        .get_agent_for_tenant(&assignee, tenant)
        .ok()
        .flatten()
        .filter(|a| a.status == "active")
        .and_then(|a| a.rig)
        .is_some_and(|r| crate::rig::is_known_rig(&r));
    if !runnable {
        return Some(mk(
            phase.to_string(),
            ACTION_ASSIGN_DECOMPOSED,
            "blocked",
            format!(
                "parent assignee `{assignee}` is not an active same-Guild Operative with a known \
                 Rig — leaving the Prime-decomposed children unassigned"
            ),
            Some(mandate_id.to_string()),
        ));
    }

    if *actions >= max {
        return Some(mk(
            phase.to_string(),
            ACTION_ASSIGN_DECOMPOSED,
            "skipped",
            "tick action budget reached".into(),
            Some(mandate_id.to_string()),
        ));
    }

    // Assign every unassigned Prime-decomposed child to the parent's assignee
    // through the EXISTING assignee primitive (resets any stale Claim, Chronicles
    // the assignment). Count ONE tick action + consume ONE bounded grant call for
    // the whole batch, only when ≥1 child was actually assigned.
    let mut assigned = 0usize;
    for k in &unassigned {
        if task_store.set_brief_field(k, "assignee", &assignee).is_ok() {
            assigned += 1;
        }
    }
    if assigned == 0 {
        return Some(mk(
            phase.to_string(),
            ACTION_ASSIGN_DECOMPOSED,
            "blocked",
            "no Prime-decomposed child Brief could be assigned".into(),
            Some(mandate_id.to_string()),
        ));
    }
    *actions += 1;
    let _ = consume_standing(agent_store, tenant, CATEGORY_ASSIGN_DECOMPOSED, now_secs);
    chronicle_autonomous(
        task_store,
        mandate_id,
        "prime.autonomous_assign_decomposed",
        &format!(
            "autonomous Prime assigned {assigned} Prime-decomposed child Brief(s) on brief \
             {parent_id} to the parent assignee {assignee}"
        ),
    );
    Some(mk(
        phase.to_string(),
        ACTION_ASSIGN_DECOMPOSED,
        "advanced",
        format!(
            "assigned {assigned} Prime-decomposed child Brief(s) to parent assignee {assignee}"
        ),
        Some(mandate_id.to_string()),
    ))
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
            run_id: None,
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
        run_id: None,
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
        run_id: None,
        plan_id: None,
        strategy_status: None,
        missing_roles: Vec::new(),
        pending_hires: Vec::new(),
        pending_clearances: Vec::new(),
        counts: BriefCounts::default(),
    }
}

/// The next completed-Shift disposition over a Mandate's OWN Brief set (Prime
/// Shift Disposition v1, company-model §12.6). Deterministic + tenant-scoped:
///
///   - APPLY takes precedence over a fresh review acceptance — an already-accepted
///     run is finished (applied) before a new one is accepted, so half-integrated
///     work is closed first.
///   - Within each kind the OLDEST eligible run wins, stable by `(started_at,
///     run_id)`, so the order is reproducible and testable.
///
/// Eligible for `review_accept`: the Brief's LATEST run is exactly `done` with
/// review exactly `pending_review` (which by construction excludes
/// failed/refused/interrupted/running/cancelled/continued and any
/// rejected/discarded/accepted/applied/conflicted/failed-apply run).
///
/// Eligible for `apply_run`: the latest run is `done`, review `accepted`, apply
/// status NOT already `applied`/`discarded`/`conflicted`/`failed`, and the
/// existing [`heartbeat::run_apply_eligibility`] passes.
///
/// Cross-tenant runs are invisible: the Brief ids are already this Guild's, and
/// each run id is re-checked with [`TaskStore::run_belongs_to_tenant`] so a
/// mis-attributed row is never selected. Returns `(phase, run_id)` or `None`.
fn disposition_candidate(
    task_store: &TaskStore,
    tenant: &str,
    brief_ids: &[String],
) -> Option<(&'static str, String)> {
    let mut apply: Vec<(i64, String)> = Vec::new();
    let mut review: Vec<(i64, String)> = Vec::new();
    for bid in brief_ids {
        let Some(run) = task_store.latest_run_for_brief(bid).ok().flatten() else {
            continue;
        };
        // Cross-tenant invisibility — never select or touch another Guild's run.
        if !task_store
            .run_belongs_to_tenant(&run.run_id, tenant)
            .unwrap_or(false)
        {
            continue;
        }
        if run.status != "done" {
            continue;
        }
        match run.review.as_deref() {
            Some("accepted") => {
                let terminal_apply = matches!(
                    run.apply_status.as_deref(),
                    Some("applied" | "discarded" | "conflicted" | "failed")
                );
                if !terminal_apply
                    && crate::nodes::coordinator::heartbeat::run_apply_eligibility(&run).is_ok()
                {
                    apply.push((run.started_at, run.run_id.clone()));
                }
            }
            Some("pending_review") => review.push((run.started_at, run.run_id.clone())),
            // rejected / discarded / anything else — a human decision, never auto.
            _ => {}
        }
    }
    let oldest = |mut v: Vec<(i64, String)>| -> Option<String> {
        v.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        v.into_iter().next().map(|(_, id)| id)
    };
    if let Some(id) = oldest(apply) {
        return Some((PHASE_NEEDS_APPLY, id));
    }
    oldest(review).map(|id| (PHASE_NEEDS_REVIEW, id))
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
            run_id: None,
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
                "needs_strategy_proposal",
                "Draft strategy",
                "This Mandate has no strategy yet. Prime can DRAFT a strategy proposal from \
                 the Mandate's goal and your company context — it is left proposed for your \
                 approval and unlocks team planning only after you approve it."
                    .into(),
                "POST /v1/spine/mandates/:id/strategy/propose",
                "mandate.strategy.propose",
                true,
                Some(ADVANCE_PROPOSE_STRATEGY),
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
                     explicit Start control, or enable autonomous Prime to start ready work — \
                     approved proposal work through prime.start, and bare-Mandate work through \
                     the same shared guarded run pipeline (budget/adapters/claims enforced).",
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
        // Prime Shift Disposition v1 (company-model §12.6 — the review→apply
        // tail). A completed Shift awaiting review acceptance (or an accepted run
        // awaiting apply) is a real governed next step over this Mandate's OWN
        // Brief set. Grant-AGNOSTIC here: `attemptable_action` gates the actual
        // autonomous action on the matching SEPARATE standing grant; with no grant
        // it stays a human gate. APPLY is surfaced before review, and before the
        // "all done" terminal, so accepted-not-applied work is never mistaken for
        // finished. The run id is carried on the step and re-validated at exec time.
        if let Some((phase, run_id)) = disposition_candidate(task_store, tenant, &brief_ids) {
            let (label, reason, api) = if phase == PHASE_NEEDS_APPLY {
                (
                    "Apply an accepted Shift",
                    format!(
                        "A completed Shift (run {run_id}) was accepted and is ready to apply \
                         through the existing safe apply path."
                    ),
                    "run.apply",
                )
            } else {
                (
                    "Review a completed Shift",
                    format!(
                        "A completed Shift (run {run_id}) is awaiting review acceptance before \
                         it can be applied."
                    ),
                    "run.review",
                )
            };
            return Ok(NextStep {
                run_id: Some(run_id),
                ..base(phase, label, reason, "/runs", api, false, None)
            });
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
    if requested != ADVANCE_CREATE_TEAM_PLAN
        && requested != ADVANCE_ORCHESTRATE
        && requested != ADVANCE_PROPOSE_STRATEGY
    {
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
        // Prime Strategy Drafting v1 — DRAFT a deterministic strategy doc from the
        // Mandate's own fields + the Guild's active work roles and propose it
        // through the EXISTING governed `mandate.strategy.propose` path. This is
        // NOT approval: the doc lands `proposed` and the next step becomes the
        // (human) strategy-approval gate.
        ADVANCE_PROPOSE_STRATEGY => {
            let mandate = match spine_store.get_mandate_for_tenant(&mandate_id, tenant) {
                Ok(Some(m)) => m,
                Ok(None) => {
                    return invalid(format!("prime.advance: mandate not found: {mandate_id}"));
                }
                Err(e) => return internal(format!("prime.advance load mandate: {e}")),
            };
            let roles = active_crew_roles(agent_store, tenant);
            let doc = draft_mandate_strategy(&mandate, &roles);
            sub.args = format!("{mandate_id}|{doc}").into_bytes();
            handle_strategy_propose(spine_store, &sub)
        }
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
/// for the caller's Guild: whether each of the categories is currently
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
        "Autonomously approve a proposed Mandate strategy through the existing mandate.strategy.approve path (a rejected/missing strategy is never approved).",
        "Autonomously accept a completed Shift's review for a Brief in this Guild's own Mandate/proposal set, through the existing review path (only a done + pending_review run; this never applies).",
        "Autonomously apply an already-accepted run through the existing safe apply machinery (run_apply_eligibility, conflict/baseline checks, review-to-done); a conflicted/failed apply never marks the Brief done.",
        "Autonomously accept/materialize an OPEN plan-package confirm that autonomous Prime itself authored, through the existing plan-confirm path + exactly-once decomposition ledger (Prime-authored packages only; a human/other-actor package is never auto-approved).",
        "Autonomously assign the unassigned child Briefs of a Prime-authored decomposition to the parent Brief's own active assignee (a known-Rig, same-Guild Operative), through the existing assignee primitive (Prime-decomposed children only; the model never picks an agent and a human/other-actor decomposition is never touched).",
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

/// Render one [`PrimeAutonomyRecord`] as the wire JSON the operator tick-now
/// surface returns. Secret-free by construction — only the bounded fields the
/// in-memory tick summary carries.
fn prime_autonomy_record_json(r: &PrimeAutonomyRecord) -> Value {
    json!({
        "tenant": r.tenant,
        "target_kind": r.target_kind,
        "target_id": r.target_id,
        "mandate_id": r.mandate_id,
        "phase": r.phase,
        "action": r.action,
        "outcome": r.outcome,
        "reason": r.reason,
        // Prime Deliberation v1 provenance: legacy/no-choice rows read as
        // deterministic_only so the operator always sees how the action was chosen.
        "ai_mode": r.ai_mode.as_deref().unwrap_or("deterministic_only"),
        "ai_reason": r.ai_reason,
        // Prime Strategy Authoring v1 provenance: present only on a propose_strategy
        // row (the strategy body's author); null elsewhere so the operator can tell
        // an LLM-authored proposed strategy from a deterministic one.
        "strategy_ai_mode": r.strategy_ai_mode,
        "strategy_ai_reason": r.strategy_ai_reason,
        // Prime Executive Prioritization v1 provenance: how the tick's candidate
        // ORDER was chosen, plus this candidate's rank in that order. Legacy/no-
        // choice rows read as deterministic_only so the operator always sees
        // whether the queue order was model-picked or deterministic.
        "priority_ai_mode": r.priority_ai_mode.as_deref().unwrap_or("deterministic_only"),
        "priority_ai_reason": r.priority_ai_reason,
        "priority_rank": r.priority_rank,
        // Prime Orchestration Authoring v1 provenance: present only on an
        // orchestrate_assign_ready row (the Brief-text author); null elsewhere so
        // the operator can tell a model-authored orchestration tree from a
        // deterministic one.
        "orchestration_ai_mode": r.orchestration_ai_mode,
        "orchestration_ai_reason": r.orchestration_ai_reason,
        // Prime Plan-Package Authoring v1 provenance + the opened package's ids /
        // child count: present only on a plan_package row; null elsewhere. The plan
        // BODY is never put on a tick record (ids/counts/summary only) — the
        // operator opens the Brief to read it.
        "plan_package_ai_mode": r.plan_package_ai_mode,
        "plan_package_ai_reason": r.plan_package_ai_reason,
        "plan_doc_id": r.plan_doc_id,
        "suggestion_id": r.suggestion_id,
        "confirm_id": r.confirm_id,
        "child_count": r.child_count,
        // The effective trigger (`tail` / `before_execute`) on a plan_package row;
        // null elsewhere.
        "plan_package_trigger": r.plan_package_trigger,
    })
}

/// `prime.autonomy_tick_now` — run EXACTLY ONE bounded autonomous Prime tick for
/// the caller's Guild on explicit operator request, and return the resulting
/// [`PrimeAutonomyRecord`] list (company-model §5.4/§8.2 — the Action Center's
/// "next governed step", here as an operator-triggered wake-up of the same
/// timer-driven driver). This makes autonomous Prime operationally legible: the
/// operator can wake the loop once and SEE what it considered / advanced /
/// started, instead of only knowing a background timer might run.
///
/// This is **still governed autonomy**, NOT a new power. It calls the SAME
/// [`autonomous_prime_tick`] path the timer uses, so every action goes through
/// the same standing-authority gates, the autonomous start budget hard-stop, the
/// Rig-readiness check, and the per-tick `RELIX_AUTONOMOUS_PRIME_MAX` bound. It
/// is scoped to the caller's OWN Guild (`Some(tenant)`) so it never drives all
/// Guilds even when the env override is on. It does **not** require the runtime
/// autonomy switch (or the env override) to be ON — an explicit operator
/// wake-up — but it grants no new authority: with no live standing grant every
/// approval gate is still left to the human exactly as before.
///
/// ROLE-GATED to operator/admin via the same `caller_is_operator` gate as
/// `prime.autonomy_set` — a worker subject is `POLICY_DENIED` with no mutation.
/// Returns `{ tenant, max, records:[…], advanced, started, considered }`; an
/// `autonomous_prime_tick` error is surfaced honestly as RESPONDER_INTERNAL.
#[allow(clippy::too_many_arguments)]
pub fn handle_prime_autonomy_tick_now(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &Arc<TaskStore>,
    registry: &crate::rig::RigRegistry,
    metrics: Option<&crate::metrics::MetricsQuery>,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    // Deterministic shape: honour the `RELIX_PRIME_LLM_DELIBERATION` switch for the
    // honest mode but carry NO live decider. The live deliberation path is wired by
    // the controller-runtime registration through
    // [`handle_prime_autonomy_tick_now_with_ai`], which builds the SAME
    // `MeshAiDecider` the background timer uses (and runs the tick from a blocking
    // thread, because that decider does a `Handle::block_on`). Callers that already
    // hold a mesh decider must use that helper, not this entry point.
    let llm_enabled = parse_prime_llm_deliberation(
        std::env::var("RELIX_PRIME_LLM_DELIBERATION")
            .ok()
            .as_deref(),
    );
    let strategy_llm_enabled = parse_prime_llm_strategy_draft(
        std::env::var("RELIX_PRIME_LLM_STRATEGY_DRAFT")
            .ok()
            .as_deref(),
    );
    let prioritization_enabled = parse_prime_llm_prioritization(
        std::env::var("RELIX_PRIME_LLM_PRIORITIZATION")
            .ok()
            .as_deref(),
    );
    let orchestration_llm_enabled = parse_prime_llm_orchestration(
        std::env::var("RELIX_PRIME_LLM_ORCHESTRATION")
            .ok()
            .as_deref(),
    );
    let plan_package_llm_enabled = parse_prime_llm_plan_package(
        std::env::var("RELIX_PRIME_LLM_PLAN_PACKAGE")
            .ok()
            .as_deref(),
    );
    let plan_package_trigger = parse_prime_plan_package_trigger(
        std::env::var("RELIX_PRIME_PLAN_PACKAGE_TRIGGER")
            .ok()
            .as_deref(),
    );
    handle_prime_autonomy_tick_now_with_ai(
        agent_store,
        spine_store,
        task_store,
        registry,
        metrics,
        ctx,
        None,
        llm_enabled,
        strategy_llm_enabled,
        prioritization_enabled,
        orchestration_llm_enabled,
        plan_package_llm_enabled,
        plan_package_trigger,
    )
}

/// Backing helper for [`handle_prime_autonomy_tick_now`] that accepts an explicit
/// optional live AI decider + the deliberation switch, so the controller-runtime
/// registration can wire the SAME [`MeshAiDecider`] the background timer uses —
/// closing the v1 caveat where the manual tick always reported `unavailable`. With
/// `ai = Some(decider)` and `llm_enabled = true` each candidate may exercise the
/// live `ai.chat` deliberation; with `ai = None` the tick is deterministic and each
/// record honestly reads `unavailable` (when `llm_enabled`) or `deterministic_only`
/// (when not). The role gate + tenant scoping are byte-for-byte the timer's: a
/// worker caller is `POLICY_DENIED` with ZERO side effects, and the tick is scoped
/// to the caller's OWN Guild (`Some(tenant)`), never all Guilds.
///
/// SAFETY: a [`MeshAiDecider`]'s `deliberate` bridges to the async mesh via
/// `Handle::block_on`, which PANICS on an async-runtime worker thread. When `ai`
/// may be a mesh decider, the caller MUST run this from a blocking thread
/// (`spawn_blocking`). The pure-decider tests pass a synchronous scripted decider
/// and so can call it directly.
#[allow(clippy::too_many_arguments)]
pub fn handle_prime_autonomy_tick_now_with_ai(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &Arc<TaskStore>,
    registry: &crate::rig::RigRegistry,
    metrics: Option<&crate::metrics::MetricsQuery>,
    ctx: &InvocationCtx,
    ai: Option<&dyn PrimeAiDecider>,
    llm_enabled: bool,
    strategy_llm_enabled: bool,
    prioritization_enabled: bool,
    orchestration_llm_enabled: bool,
    plan_package_llm_enabled: bool,
    plan_package_trigger: PrimePlanPackageTrigger,
) -> HandlerOutcome {
    // Same Board-only gate as the runtime toggle: a worker subject can never
    // wake the autonomous Prime driver, even though this takes no new authority.
    if !caller_is_operator(ctx) {
        return policy_denied(
            "prime.autonomy_tick_now is operator/admin-only — a worker subject cannot wake \
             autonomous Prime"
                .to_string(),
        );
    }
    let tenant = ctx.tenant_id_or_default();
    // Live per-tick bound + safe hire Rig — the SAME knobs the timer reads, so a
    // manual wake-up never exceeds the configured action budget.
    let max = crate::nodes::coordinator::heartbeat::parse_autonomous_prime_max(
        std::env::var("RELIX_AUTONOMOUS_PRIME_MAX").ok().as_deref(),
    );
    let hire_rig = configured_autonomous_hire_rig();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    // Tenant-scoped (`Some(tenant)`): this wakes ONLY the caller's Guild, never
    // all Guilds — even if the env override is on for the background timer. When a
    // live decider is wired (mesh AI peer reachable) and `RELIX_PRIME_LLM_DELIBERATION`
    // is on, each candidate may use the live deliberation layer; otherwise the
    // deterministic action runs and the record honestly reads `unavailable` /
    // `deterministic_only`.
    let records = match autonomous_prime_tick(
        agent_store,
        spine_store,
        task_store,
        registry,
        metrics,
        now_ms,
        max,
        Some(tenant),
        &hire_rig,
        ai,
        llm_enabled,
        strategy_llm_enabled,
        prioritization_enabled,
        orchestration_llm_enabled,
        plan_package_llm_enabled,
        plan_package_trigger,
    ) {
        Ok(r) => r,
        Err(e) => return internal(format!("prime.autonomy_tick_now: {e}")),
    };
    let advanced = records.iter().filter(|r| r.outcome == "advanced").count();
    let started = records.iter().filter(|r| r.outcome == "started").count();
    let rows: Vec<Value> = records.iter().map(prime_autonomy_record_json).collect();
    ok_json(&json!({
        "tenant": tenant,
        "max": max,
        "records": rows,
        "advanced": advanced,
        "started": started,
        "considered": records.len(),
    }))
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
// work — for an already-approved proposal through the existing `prime.start`
// path, and for a BARE Mandate (no owning proposal) through the same shared
// guarded run pipeline (claims, adapter probe, durable ledger, budget hard-stop),
// stamped as an autonomous/heartbeat-trigger run. By DEFAULT it NEVER
// auto-approves a strategy / proposal / hire / spawn /
// budget / Clearance gate (those stay human); the ONLY exception is the
// **standing-authority layer** below — a gate is approved only while the Board
// holds a live `standing_approvals` grant for the matching category in THAT
// Guild (`prime.proposal.approve` / `prime.hire.approve` /
// `prime.clearance.approve` / `prime.strategy.approve`), and budget approvals are
// never delegated. Bounded per tick, idempotent (each tick re-classifies, so team
// plans / orchestration trees / started Shifts never duplicate), and tenant-safe
// (each candidate is processed under its OWN Guild).
// ─────────────────────────────────────────────────────────────────────────

// ── PRIME DELIBERATION v1 (opt-in constrained model choice) ─────────────────
// The autonomous loop is no longer a hardcoded deterministic state machine when
// `RELIX_PRIME_LLM_DELIBERATION` is on: for each candidate the loop computes the
// SINGLE legal next governed action (exactly as before), then — only as an
// advisory pre-pass — asks an opt-in model to either CONFIRM that action or HOLD
// (`none`) this tick. THE MODEL IS NOT THE PERMISSION SYSTEM: it can only pick
// from `[<computed action>, none]`, it can never invent an action or pick one
// outside the candidate's allowed set, and every action it confirms still flows
// through the same governed handler + standing authority + budget gate + claim +
// adapter probe + tenant isolation. Malformed / disallowed / unavailable model
// output degrades to the deterministic behaviour with an honest mode. No
// provider key ever enters the coordinator: the live decider only performs the
// existing `ai.chat` mesh call to the AI peer.

/// Parse `RELIX_PRIME_LLM_DELIBERATION` (`1|true|yes|on`, case-insensitive) into
/// the deliberation-enabled flag. Default OFF — absent/blank/anything else.
pub fn parse_prime_llm_deliberation(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Parse `RELIX_PRIME_LLM_STRATEGY_DRAFT` (`1|true|yes|on`, case-insensitive) into
/// the model-strategy-authoring flag (Prime Strategy Authoring v1). Default OFF.
/// Independent of `RELIX_PRIME_LLM_DELIBERATION`: a Guild may let the model author
/// the *proposed* strategy body while keeping deterministic action selection (or
/// vice versa). Either way the strategy is only ever PROPOSED, never approved by
/// the model.
pub fn parse_prime_llm_strategy_draft(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Parse `RELIX_PRIME_LLM_ORCHESTRATION` (`1|true|yes|on`, case-insensitive) into
/// the model-orchestration-authoring flag (Prime Orchestration Authoring v1).
/// Default OFF. Independent of the other Prime LLM switches: a Guild may let the
/// model author the orchestration Brief *text* (titles / dossiers / checklists)
/// for the already-computed skeleton while keeping deterministic action selection
/// / strategy drafting / prioritization. The model authors text only — it never
/// invents a role, agent, Brief id, assignment, or gate, and any
/// invalid/unavailable output falls back to the deterministic titles + dossiers.
pub fn parse_prime_llm_orchestration(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Parse `RELIX_PRIME_LLM_PLAN_PACKAGE` (`1|true|yes|on`, case-insensitive) into
/// the model-plan-package-authoring flag (Prime Plan-Package Authoring v1).
/// Default OFF. Independent of the other Prime LLM switches: a Guild may let the
/// model author a *proposed* Brief decomposition (plan Dossier + `suggest_tasks`
/// proposal + approval-bound `confirm`) for an un-decomposed Brief while keeping
/// deterministic action selection / strategy drafting / orchestration / priority.
/// The model authors plan content only — it never assigns agents, picks
/// tools/methods, mutates an existing Dossier, or approves anything; the confirm is
/// always left OPEN for a human, and any invalid/unavailable output falls back to a
/// deterministic safe decomposition. With the switch OFF the tick authors no plan
/// package at all (byte-for-byte legacy behaviour).
pub fn parse_prime_llm_plan_package(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Parse `RELIX_PRIME_PLAN_PACKAGE_TRIGGER` into the effective
/// [`PrimePlanPackageTrigger`] (Prime Active Planner Trigger v2). Layered ON TOP of
/// the master [`parse_prime_llm_plan_package`] opt-in: this only decides WHEN
/// authoring fires, never WHETHER. `tail` / `gap_fill` / blank → `Tail` (v1 idle
/// gap-fill only); `before_execute` / `plan_before_execute` → `BeforeExecute` (also
/// open a package BEFORE starting a lone eligible un-decomposed Brief, holding the
/// raw start for a human). Any UNKNOWN value safely falls back to `Tail`. With the
/// master switch OFF this is inert — NO plan-package authoring happens in any mode.
pub fn parse_prime_plan_package_trigger(raw: Option<&str>) -> PrimePlanPackageTrigger {
    PrimePlanPackageTrigger::parse(raw)
}

/// The AI-peer mesh alias the live deliberation decider calls: `RELIX_PRIME_AI_PEER`,
/// default `ai` (the same alias the bridge's chat seam uses).
pub fn prime_ai_peer() -> String {
    std::env::var("RELIX_PRIME_AI_PEER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ai".to_string())
}

/// The `ai.chat` session id the live deliberation decider scopes its model
/// conversation under: `RELIX_PRIME_LLM_SESSION`, default `prime-autonomy`.
pub fn prime_llm_session() -> String {
    std::env::var("RELIX_PRIME_LLM_SESSION")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "prime-autonomy".to_string())
}

/// The live AI decision provider: bridges the synchronous tick (it runs inside
/// `spawn_blocking`) to the async mesh `ai.chat` call, using the SAME
/// `{session_id, prompt, history}` JSON shape as the bridge's `call_ai_chat` and
/// the SAME governed mesh client the coordinator already holds. It performs NO
/// governed action — it only returns the model's raw reply for the validator to
/// vet. A missing peer / transport failure surfaces as an honest error so the
/// loop records `unavailable` and falls back deterministically. Construction
/// requires a coordinator mesh client + identity bundle; when those are absent
/// the loop simply passes `None` and every tick is deterministic.
pub struct MeshAiDecider {
    handle: tokio::runtime::Handle,
    mesh: crate::manifest::MeshClient,
    identity: relix_core::bundle::Bundle,
    alias: String,
    session: String,
    deadline_secs: i64,
    tenant: Option<String>,
}

impl MeshAiDecider {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        handle: tokio::runtime::Handle,
        mesh: crate::manifest::MeshClient,
        identity: relix_core::bundle::Bundle,
        alias: String,
        session: String,
        deadline_secs: i64,
        tenant: Option<String>,
    ) -> Self {
        Self {
            handle,
            mesh,
            identity,
            alias,
            session,
            // Clamp to the same 5..=60s band the bridge's chat seam uses so a
            // misconfigured deadline can never block the loop forever.
            deadline_secs: deadline_secs.clamp(5, 60),
            tenant,
        }
    }
}

impl PrimeAiDecider for MeshAiDecider {
    fn deliberate(&self, prompt: &str) -> Result<String, String> {
        let arg = json!({
            "session_id": self.session,
            "prompt": prompt,
            "history": "",
        });
        let arg_bytes = serde_json::to_vec(&arg).map_err(|e| format!("encode ai.chat arg: {e}"))?;
        let envelope = crate::dispatch::build_request_with_tenant(
            "ai.chat",
            arg_bytes,
            self.identity.clone(),
            self.deadline_secs,
            None,
            None,
            None,
            self.tenant.clone(),
        );
        let mesh = self.mesh.clone();
        let alias = self.alias.clone();
        // Bridge the async mesh call into this synchronous (spawn_blocking) tick.
        let resp_bytes = self
            .handle
            .block_on(async move { mesh.call(&alias, envelope).await })
            .map_err(|e| format!("ai peer unreachable: {e}"))?;
        let resp = crate::dispatch::decode_response(&resp_bytes)
            .map_err(|e| format!("ai.chat decode: {e}"))?;
        match resp.res {
            crate::transport::envelope::ResponseResult::Ok(body) => {
                let text = String::from_utf8_lossy(&body).trim().to_string();
                if text.is_empty() {
                    Err("model returned an empty reply".to_string())
                } else {
                    Ok(text)
                }
            }
            crate::transport::envelope::ResponseResult::Err(env) => {
                Err(format!("ai.chat responder error: {}", env.cause))
            }
            crate::transport::envelope::ResponseResult::StreamHandle(_) => {
                Err("ai.chat returned a stream handle".to_string())
            }
        }
    }
}

/// The ONE governed action the deterministic classifier would take for a
/// classified step — the only positive choice the deliberation layer may offer
/// the model (alongside `none`). `None` for a step that is a pure human gate /
/// running / done state where the loop takes no autonomous action.
fn intended_action(step: &NextStep) -> Option<&'static str> {
    // Auto-advanceable steps carry their explicit advance key
    // (create_team_plan / orchestrate_assign_ready / propose_strategy).
    if step.can_advance {
        return step.advance_action;
    }
    match step.phase {
        "ready_to_start" => Some(if step.proposal_id.is_some() {
            "start"
        } else {
            "start_mandate"
        }),
        "needs_hire_approval" => {
            if !step.pending_clearances.is_empty() {
                Some("clearance_approve")
            } else if !step.pending_hires.is_empty() {
                Some("hire_approve")
            } else {
                None
            }
        }
        "needs_approval"
            if step.action_api == "mandate.strategy.approve"
                && step.strategy_status.as_deref() == Some("proposed") =>
        {
            Some("approve_strategy")
        }
        // Shift disposition — only actionable with a concrete classified run id.
        PHASE_NEEDS_REVIEW => step.run_id.as_ref().map(|_| ACTION_REVIEW_ACCEPT),
        PHASE_NEEDS_APPLY => step.run_id.as_ref().map(|_| ACTION_APPLY_RUN),
        _ => None,
    }
}

/// Build the bounded, secret-free deliberation snapshot from a classified step.
fn snapshot_from_step(
    tenant: &str,
    kind: &str,
    target_id: &str,
    step: &NextStep,
    action: &str,
) -> PrimeDeliberationInput {
    PrimeDeliberationInput {
        tenant: tenant.to_string(),
        target_kind: kind.to_string(),
        target_id: target_id.to_string(),
        mandate_id: step.mandate_id.clone(),
        phase: step.phase.to_string(),
        computed_action: action.to_string(),
        reason: step.reason.clone(),
        strategy_status: step.strategy_status.clone(),
        total_briefs: step.counts.total,
        ready: step.counts.ready,
        unassigned: step.counts.unassigned,
        running: step.counts.running,
        needs_review: step.counts.needs_review,
        blocked: step.counts.blocked,
        missing_roles: step.missing_roles.len(),
        pending_hires: step.pending_hires.len(),
        pending_clearances: step.pending_clearances.len(),
    }
}

/// The outcome of consulting the deliberation layer for one candidate action.
enum Deliberation {
    /// The model chose `none` — HOLD this tick: execute nothing.
    Hold {
        mode: PrimeDeliberationMode,
        reason: Option<String>,
    },
    /// Proceed with the deterministic action (model confirmed it, or fell back).
    Proceed {
        mode: PrimeDeliberationMode,
        reason: Option<String>,
    },
}

fn non_empty(s: String) -> Option<String> {
    if s.trim().is_empty() { None } else { Some(s) }
}

/// Consult the optional model for ONE candidate action. The model may only
/// confirm `snap.computed_action` or choose `none`; anything else is rejected and
/// degrades to Proceed (deterministic). Never executes a governed action.
fn deliberate(ai: Option<&dyn PrimeAiDecider>, snap: &PrimeDeliberationInput) -> Deliberation {
    let Some(decider) = ai else {
        return Deliberation::Proceed {
            mode: PrimeDeliberationMode::Unavailable,
            reason: Some("no AI decider wired for this tick".to_string()),
        };
    };
    let prompt = build_prime_deliberation_prompt(snap);
    match decider.deliberate(&prompt) {
        Ok(raw) => match parse_prime_decision(&raw, &snap.allowed_actions()) {
            Ok(d) if d.action == ACTION_NONE => Deliberation::Hold {
                mode: PrimeDeliberationMode::LlmUsed,
                reason: non_empty(d.reason),
            },
            // A valid choice equal to the computed action — confirmed.
            Ok(d) => Deliberation::Proceed {
                mode: PrimeDeliberationMode::LlmUsed,
                reason: non_empty(d.reason),
            },
            Err(e) => Deliberation::Proceed {
                mode: PrimeDeliberationMode::Fallback,
                reason: Some(format!("model output rejected: {e}")),
            },
        },
        Err(e) => Deliberation::Proceed {
            mode: PrimeDeliberationMode::Unavailable,
            reason: Some(format!("model unavailable: {e}")),
        },
    }
}

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
    /// `propose_strategy` / `approve_strategy` / `approve` / `hire_approve` /
    /// `clearance_approve` / `start` (approved proposal) / `start_mandate` (bare
    /// Mandate ready work) / `review_accept` (accept a completed Shift's review) /
    /// `apply_run` (apply an accepted run) / `none`.
    pub action: &'static str,
    /// `advanced` / `started` / `skipped` / `blocked`.
    pub outcome: &'static str,
    /// A short, secret-free reason for the outcome.
    pub reason: String,
    /// How this tick's action choice was made (Prime Deliberation v1):
    /// `deterministic_only` / `llm_used` / `fallback` / `unavailable`. `None` is
    /// treated as `deterministic_only` by the renderer (legacy rows).
    pub ai_mode: Option<String>,
    /// The model's short reason when it participated (`llm_used`), or the honest
    /// reason it was not used (`fallback` / `unavailable`). Secret-free.
    pub ai_reason: Option<String>,
    /// Prime Strategy Authoring v1 provenance — how the PROPOSED strategy *body*
    /// was authored on a `propose_strategy` action: `deterministic_only` /
    /// `llm_used` / `fallback` / `unavailable`. `None` for every other action (no
    /// strategy was drafted). This is distinct from `ai_mode`, which is the
    /// *action-choice* provenance.
    pub strategy_ai_mode: Option<String>,
    /// Secret-free reason the strategy body was authored the way it was (model
    /// reason on `llm_used`, or the honest fallback/unavailable reason). `None`
    /// when no strategy was drafted.
    pub strategy_ai_reason: Option<String>,
    /// Prime Executive Prioritization v1 provenance — how this tick's CANDIDATE
    /// ORDER (which legal candidate the bounded tick spent its action budget on)
    /// was chosen: `deterministic_only` / `llm_used` / `fallback` / `unavailable`.
    /// `None` is treated as `deterministic_only` by the renderer (legacy rows).
    /// This is distinct from `ai_mode` (the per-candidate action-choice
    /// provenance) and `strategy_ai_mode` (the strategy-body author).
    pub priority_ai_mode: Option<String>,
    /// The model's short reason for the queue order when it participated
    /// (`llm_used`), or the honest reason it was not used (`fallback` /
    /// `unavailable`). Secret-free. Shared across every record in the same tick.
    pub priority_ai_reason: Option<String>,
    /// The candidate's 1-based rank in the chosen execution order when the model
    /// picked the order (`priority_ai_mode == llm_used`); `None` in deterministic
    /// modes or for a candidate that was not part of the offered actionable menu.
    pub priority_rank: Option<usize>,
    /// Prime Orchestration Authoring v1 provenance — how the orchestration Brief
    /// TEXT (titles / dossiers / checklists) was authored on an
    /// `orchestrate_assign_ready` action: `deterministic_only` / `llm_used` /
    /// `fallback` / `unavailable`. `None` for every other action (no orchestration
    /// text was authored). Distinct from `ai_mode` (action choice),
    /// `strategy_ai_mode` (strategy body), and `priority_ai_mode` (queue order).
    pub orchestration_ai_mode: Option<String>,
    /// Secret-free reason the orchestration text was authored the way it was
    /// (model reason on `llm_used`, or the honest fallback/unavailable reason).
    /// `None` when no orchestration text was authored.
    pub orchestration_ai_reason: Option<String>,
    /// Prime Plan-Package Authoring v1 provenance — how the proposed plan-package
    /// CONTENT (plan title/body, summary, child Briefs) was authored on a
    /// `plan_package` action: `deterministic_only` / `llm_used` / `fallback` /
    /// `unavailable`. `None` for every other action (no plan package was authored).
    /// Distinct from the other `*_ai_mode` fields.
    pub plan_package_ai_mode: Option<String>,
    /// Secret-free reason the plan-package content was authored the way it was
    /// (model reason on `llm_used`, or the honest fallback/unavailable reason).
    /// `None` when no plan package was authored.
    pub plan_package_ai_reason: Option<String>,
    /// The immutable `plan` Dossier revision id opened by a `plan_package` action;
    /// `None` for every other action. (Ids/counts only — the plan body never goes
    /// into a tick record.)
    pub plan_doc_id: Option<String>,
    /// The `suggest_tasks` proposal id opened by a `plan_package` action; `None`
    /// otherwise.
    pub suggestion_id: Option<String>,
    /// The approval-bound `confirm` id opened by a `plan_package` action (left OPEN
    /// for the operator); `None` otherwise.
    pub confirm_id: Option<String>,
    /// How many child Briefs the proposed plan package would create on approval;
    /// `None` for every other action.
    pub child_count: Option<usize>,
    /// The EFFECTIVE plan-package trigger that produced a `plan_package` action:
    /// `tail` (v1 idle gap-fill) or `before_execute` (v2 active planner — opened
    /// BEFORE a raw Brief start). Reflects the configured
    /// `RELIX_PRIME_PLAN_PACKAGE_TRIGGER` after unknown-value fallback to `tail`.
    /// `None` for every other action (no plan package was authored).
    pub plan_package_trigger: Option<String>,
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

/// The STABLE Chronicle anchor Brief for a Mandate: a top-level (non-Sub-brief)
/// Brief chosen deterministically (lowest `task_id`) so EVERY autonomous event
/// for the Mandate lands on the SAME Brief across ticks. The natural
/// `list_briefs_by_mandate` order is `updated_at DESC` — the most-recently-touched
/// Brief, with arbitrary tie-breaking inside a second — so anchoring on its first
/// row scattered an action's Chronicle onto whatever Sub-brief happened to be
/// written last (e.g. a just-materialized decomposition child), making provenance
/// non-deterministic. Restricting to top-level Briefs keeps the anchor on the
/// Mandate's parent/orchestration-root across decomposition and orchestration (a
/// Sub-brief never captures it); the `min` tie-break is stable as task ids are
/// immutable. Falls back to the lowest-id Brief if every Brief is somehow a
/// Sub-brief. `None` when the Mandate has no Brief yet.
pub(crate) fn mandate_chronicle_anchor(task_store: &TaskStore, mandate_id: &str) -> Option<String> {
    let briefs = task_store.list_briefs_by_mandate(mandate_id, 500).ok()?;
    let mut top_level: Vec<String> = Vec::new();
    let mut all: Vec<String> = Vec::new();
    for c in &briefs {
        all.push(c.task_id.clone());
        let is_sub = task_store
            .parent_briefs(&c.task_id)
            .map(|p| !p.is_empty())
            .unwrap_or(false);
        if !is_sub {
            top_level.push(c.task_id.clone());
        }
    }
    if !top_level.is_empty() {
        top_level.into_iter().min()
    } else {
        all.into_iter().min()
    }
}

/// Append ONE Chronicle event for an actual autonomous action onto the Mandate's
/// stable anchor Brief ([`mandate_chronicle_anchor`]) — sparingly, only when a
/// Brief exists. No Brief yet (e.g. a team-plan before orchestration) →
/// record-only, no event, so an idle loop never spams the Chronicle.
fn chronicle_autonomous(task_store: &TaskStore, mandate_id: &str, event_type: &str, detail: &str) {
    if let Some(anchor) = mandate_chronicle_anchor(task_store, mandate_id) {
        let _ = task_store.append_event(&anchor, event_type, detail);
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

/// Autonomously start the currently-ready Briefs of a BARE Mandate — one that
/// reached `ready_to_start` with NO owning Prime proposal — through the SAME
/// guarded run pipeline the heartbeat dispatcher and `prime.start` use
/// ([`heartbeat::preflight_and_spawn_with_trigger`] → `preflight_run_with_prefs_trigger`
/// → `prepare_claimed_run` → `execute_ready`): the single-owner Claim, the
/// duplicate-run guard, the live adapter probe, scoped workspace prep, the durable
/// `brief_runs` ledger row, bridge-token minting, board advancement, and Chronicle
/// events. NO second run system is invented — the only differences from a manual
/// `brief.run` are (1) the run trigger is [`RunTrigger::Heartbeat`] (the autonomous
/// boundary, not dashboard `manual`) and (2) the per-Brief autonomous budget
/// hard-stop is applied FIRST.
///
/// Tenant-isolated: the ready set is read with
/// [`TaskStore::list_ready_briefs_for_tenant`] for the candidate's OWN Guild and
/// filtered to `mandate_id`, so no cross-Guild Brief is ever selected or started.
/// Budget hard-stop: if ANY ready same-tenant Brief of the Mandate is over budget
/// ([`heartbeat::dispatch_budget_admits`]), the WHOLE autonomous start is blocked
/// and ZERO runs open (conservative, mirroring the proposal start gate). Returns
/// `(outcome, reason, started_count)`.
#[allow(clippy::too_many_arguments)]
fn start_bare_mandate_ready(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &Arc<TaskStore>,
    registry: &crate::rig::RigRegistry,
    metrics: Option<&crate::metrics::MetricsQuery>,
    now_ms: i64,
    tenant: &str,
    mandate_id: &str,
) -> (&'static str, String, usize) {
    use crate::nodes::coordinator::heartbeat::{
        BudgetAdmission, DEFAULT_DISPATCH_LEASE_SECS, RunModelPrefs, RunTrigger,
        dispatch_budget_admits, preflight_and_spawn_with_trigger,
    };

    // Tenant-scoped ready set, narrowed to THIS Mandate. `list_ready_briefs_for_tenant`
    // already excludes unassigned / blocked / live-claimed Briefs, so a Brief another
    // run currently owns is not even a candidate (never double-started).
    let ready: Vec<_> = task_store
        .list_ready_briefs_for_tenant(tenant, 500)
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.mandate_id.as_deref() == Some(mandate_id))
        .collect();
    if ready.is_empty() {
        return (
            "skipped",
            "no ready same-tenant Brief to start for this Mandate".into(),
            0,
        );
    }

    // Autonomous budget hard-stop: if ANY ready Brief is over budget, refuse the
    // whole start and open ZERO runs (the same conservative gate the proposal start
    // applies; the heartbeat still gates per Brief). Inert when metrics/spine carry
    // no budget signal, mirroring the heartbeat dispatcher.
    for card in &ready {
        if let BudgetAdmission::Refuse { reason, .. } = dispatch_budget_admits(
            card,
            task_store,
            agent_store,
            Some(spine_store),
            metrics,
            now_ms,
        ) {
            return ("blocked", format!("budget hard-stop: {reason}"), 0);
        }
    }

    // Start each ready Brief through the shared guarded pipeline, stamped as an
    // autonomous (heartbeat-trigger) run. A pre-run refusal (adapter unavailable /
    // Claim lost) is recorded honestly as a tenant-scoped refusal and not counted.
    let bridge_tokens = crate::rig::bridge::BridgeTokenStore::global();
    let mut started = 0usize;
    for card in &ready {
        let brief_id = &card.task_id;
        let assignee = card.assignee_agent_id.clone().unwrap_or_default();
        let agent = agent_store
            .get_agent_for_tenant(&assignee, tenant)
            .ok()
            .flatten();
        let prefs = agent
            .as_ref()
            .map(|a| RunModelPrefs::new(a.model_preference.clone(), a.reasoning_effort.clone()))
            .unwrap_or_default();
        let preferred = agent.as_ref().and_then(|a| a.rig.clone());
        let charter = agent
            .map(|a| a.instruction_bundle)
            .filter(|c| !c.trim().is_empty());
        let prompt = task_store.compose_brief_prompt_with_charter(brief_id, 10, charter.as_deref());
        match preflight_and_spawn_with_trigger(
            task_store,
            registry,
            Some(&bridge_tokens),
            DEFAULT_DISPATCH_LEASE_SECS,
            brief_id,
            preferred.as_deref(),
            prompt,
            prefs,
            RunTrigger::Heartbeat,
        ) {
            // A Shift started (run_id present).
            Ok(report) if report.run_id.is_some() => {
                let _ = task_store.append_event(
                    brief_id,
                    "prime.work_started",
                    &format!(
                        "autonomous Prime started work on `{}` (run {})",
                        report.rig,
                        report.run_id.as_deref().unwrap_or("")
                    ),
                );
                started += 1;
            }
            // A pre-run refusal — durable tenant-scoped refusal, never a faked run.
            Ok(report) => {
                let _ = task_store.record_manual_refusal_for_tenant(
                    brief_id,
                    tenant,
                    &assignee,
                    &report.rig,
                    &report.status,
                    &report.summary,
                );
            }
            Err(_) => {}
        }
    }

    if started > 0 {
        (
            "started",
            format!("started {started} ready Shift(s)"),
            started,
        )
    } else {
        ("skipped", "no ready Shift actually started".into(), 0)
    }
}

/// Process ONE autonomous candidate (Prime Deliberation v1 wrapper). First
/// computes the SINGLE legal next governed action exactly as before, then — when
/// `llm_enabled` and the candidate has a positive action — optionally lets the
/// model CONFIRM that action or HOLD (`none`) this tick. The model can never
/// widen the choice or bypass a gate: a `none` skips with zero side effects; a
/// confirm (or any malformed/unavailable output) runs the deterministic
/// [`process_candidate_inner`], whose governed handlers + standing authority +
/// budget + claims + adapter probes are unchanged. Stamps the deliberation
/// provenance (`ai_mode` / `ai_reason`) on the record.
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
    ai: Option<&dyn PrimeAiDecider>,
    llm_enabled: bool,
    strategy_llm_enabled: bool,
    orchestration_llm_enabled: bool,
    plan_package_llm_enabled: bool,
    plan_package_trigger: PrimePlanPackageTrigger,
) -> PrimeAutonomyRecord {
    let mut delib_mode = PrimeDeliberationMode::DeterministicOnly;
    let mut delib_reason: Option<String> = None;
    if llm_enabled {
        let read_ctx = autonomous_prime_ctx(tenant, target.to_string().into_bytes());
        if let Ok(step) = compute_next_step(agent_store, spine_store, task_store, &read_ctx)
            && let Some(action) = intended_action(&step)
        {
            let snap = snapshot_from_step(tenant, kind, target_id, &step, action);
            match deliberate(ai, &snap) {
                // The model declined to act this tick — execute NOTHING.
                Deliberation::Hold { mode, reason } => {
                    return PrimeAutonomyRecord {
                        tenant: tenant.to_string(),
                        target_kind: kind,
                        target_id: target_id.to_string(),
                        mandate_id: step.mandate_id.clone(),
                        phase: step.phase.to_string(),
                        action: "none",
                        outcome: "skipped",
                        reason: match &reason {
                            Some(r) => format!("model chose to hold (none): {r}"),
                            None => "model chose to hold (none)".to_string(),
                        },
                        ai_mode: Some(mode.as_str().to_string()),
                        ai_reason: reason,
                        strategy_ai_mode: None,
                        strategy_ai_reason: None,
                        priority_ai_mode: None,
                        priority_ai_reason: None,
                        priority_rank: None,
                        orchestration_ai_mode: None,
                        orchestration_ai_reason: None,
                        plan_package_ai_mode: None,
                        plan_package_ai_reason: None,
                        plan_doc_id: None,
                        suggestion_id: None,
                        confirm_id: None,
                        child_count: None,
                        plan_package_trigger: None,
                    };
                }
                Deliberation::Proceed { mode, reason } => {
                    delib_mode = mode;
                    delib_reason = reason;
                }
            }
        }
    }

    let mut rec = process_candidate_inner(
        agent_store,
        spine_store,
        task_store,
        registry,
        metrics,
        now_ms,
        tenant,
        kind,
        target_id,
        target,
        actions,
        max,
        hire_rig,
        ai,
        strategy_llm_enabled,
        orchestration_llm_enabled,
        plan_package_llm_enabled,
        plan_package_trigger,
    );
    rec.ai_mode = Some(delib_mode.as_str().to_string());
    rec.ai_reason = delib_reason;
    rec
}

/// The deterministic body: classify the candidate's next governed step and,
/// when it is a safe auto-advanceable / ready-to-start / grant-covered step,
/// execute exactly that one step through the existing governed handler / shared
/// run pipeline. Counts a real mutation against `actions` (so the tick stays
/// bounded by `max`); a human gate / already-running / done step records and acts
/// on nothing. UNCHANGED by deliberation — the wrapper only chooses whether to
/// call it.
#[allow(clippy::too_many_arguments)]
fn process_candidate_inner(
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
    ai: Option<&dyn PrimeAiDecider>,
    strategy_llm_enabled: bool,
    orchestration_llm_enabled: bool,
    plan_package_llm_enabled: bool,
    plan_package_trigger: PrimePlanPackageTrigger,
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
            // The wrapper [`process_candidate`] stamps the real deliberation
            // provenance; the deterministic inner defaults to none.
            ai_mode: None,
            ai_reason: None,
            // Set only on the propose_strategy arm below (the strategy author).
            strategy_ai_mode: None,
            strategy_ai_reason: None,
            // The tick's prioritization layer stamps these after execution.
            priority_ai_mode: None,
            priority_ai_reason: None,
            priority_rank: None,
            // Set only on the orchestrate_assign_ready arm below (the text author).
            orchestration_ai_mode: None,
            orchestration_ai_reason: None,
            plan_package_ai_mode: None,
            plan_package_ai_reason: None,
            plan_doc_id: None,
            suggestion_id: None,
            confirm_id: None,
            child_count: None,
            plan_package_trigger: None,
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

        // (A1) Prime Strategy Authoring v1 — when the safe advance is
        // `propose_strategy`, author the strategy *body* here (model-authored when
        // the strategy flag is on AND a live decider is wired, else deterministic)
        // and propose it through the EXISTING governed `mandate.strategy.propose`
        // handler. The classification above only yields this advance for a Mandate
        // with NO strategy yet (`needs_strategy_proposal`), so an existing
        // proposed/approved/rejected strategy is NEVER overwritten — exactly the
        // stale-guard the deterministic `handle_prime_advance` enforces. Drafting is
        // NOT approval: the doc lands `proposed`; the strategy provenance is stamped
        // on the record so the operator sees whether the model authored it.
        if action == ADVANCE_PROPOSE_STRATEGY {
            let Some(mid) = mandate_id.clone() else {
                return mk(
                    phase,
                    action,
                    "skipped",
                    "propose_strategy: next step has no mandate".into(),
                    None,
                );
            };
            let mandate = match spine_store.get_mandate_for_tenant(&mid, tenant) {
                Ok(Some(m)) => m,
                Ok(None) => {
                    return mk(
                        phase,
                        action,
                        "blocked",
                        format!("propose_strategy: mandate not found: {mid}"),
                        Some(mid),
                    );
                }
                Err(e) => {
                    return mk(
                        phase,
                        action,
                        "blocked",
                        format!("propose_strategy load mandate: {e}"),
                        Some(mid),
                    );
                }
            };
            let roles = active_crew_roles(agent_store, tenant);
            let draft = draft_strategy_doc(
                ai,
                &mandate,
                &roles,
                Some(&step.counts),
                strategy_llm_enabled,
            );
            let propose_ctx =
                autonomous_prime_ctx(tenant, format!("{mid}|{}", draft.doc).into_bytes());
            return match handle_strategy_propose(spine_store, &propose_ctx) {
                HandlerOutcome::Ok(_) => {
                    *actions += 1;
                    chronicle_autonomous(
                        task_store,
                        &mid,
                        "prime.autonomous_strategy_proposed",
                        &format!(
                            "autonomous Prime drafted a strategy proposal for mandate {mid} (body: {})",
                            draft.mode.as_str()
                        ),
                    );
                    let mut rec = mk(
                        phase,
                        action,
                        "advanced",
                        format!("proposed a {} strategy draft", draft.mode.as_str()),
                        Some(mid),
                    );
                    rec.strategy_ai_mode = Some(draft.mode.as_str().to_string());
                    rec.strategy_ai_reason = draft.reason;
                    rec
                }
                // Governance / store refusal — propagate honestly, take no credit.
                HandlerOutcome::Err(e) => mk(
                    phase,
                    action,
                    "blocked",
                    format!("strategy propose refused: {}", e.cause),
                    Some(mid),
                ),
            };
        }

        // (A2) Prime Orchestration Authoring v1 — when the safe advance is
        // `orchestrate_assign_ready`, author the Brief TEXT (titles / dossiers /
        // checklists) for the already-computed skeleton (model-authored when the
        // orchestration flag is on AND a live decider is wired, else deterministic)
        // and materialise the tree through the EXISTING governed
        // `handle_orchestrate_with_blueprint` in `assign_ready` mode. The blueprint
        // is TEXT-ONLY and fully re-validated/key-constrained server-side: it
        // cannot invent a role, agent, Brief id, assignment, dependency, or gate —
        // every orchestration gate (approved strategy, ready team, assign-Key,
        // reviewer stamping, max_briefs cap, placeholder behaviour, source-marker
        // idempotency) is identical to the deterministic path. Bad / unavailable
        // output falls back to deterministic text. The provenance is stamped on the
        // record so the operator sees whether the model authored the tree text.
        if action == ADVANCE_ORCHESTRATE {
            let Some(mid) = mandate_id.clone() else {
                return mk(
                    phase,
                    action,
                    "skipped",
                    "orchestrate: next step has no mandate".into(),
                    None,
                );
            };
            // (A2-pre) Prime-Decomposed Child Assignment — Standing Authority v1.
            // BEFORE running orchestration (which builds its own skeleton and
            // canNOT adopt Prime-decomposed children, so it only no-ops here), if
            // this Mandate carries unassigned child Briefs under a parent whose
            // plan package autonomous Prime ITSELF authored AND the Board granted
            // `prime.brief.assign_decomposed`, assign those children to the
            // parent's own active assignee through the existing assignee primitive.
            // With no grant this is a no-op (`None`) and orchestration parks
            // honestly at the assignment gate exactly as before; a human/other-actor
            // decomposition is never touched. Never lets the model pick an agent.
            if let Some(rec) = try_assign_decomposed_children_for_mandate(
                agent_store,
                task_store,
                tenant,
                &mid,
                now_ms,
                actions,
                max,
                &phase,
                &mk,
            ) {
                return rec;
            }
            let (blueprint, orch_mode, orch_reason) = author_orchestration_blueprint(
                ai,
                agent_store,
                spine_store,
                tenant,
                &mid,
                orchestration_llm_enabled,
            );
            let orch_ctx = autonomous_prime_ctx(tenant, format!("{mid}|assign_ready").into_bytes());
            return match handle_orchestrate_with_blueprint(
                task_store,
                agent_store,
                spine_store,
                &orch_ctx,
                blueprint.as_ref(),
            ) {
                HandlerOutcome::Ok(_) => {
                    // Idempotent no-op detection. An `assign_ready` run that made
                    // no structural change did no real work this tick. Without this
                    // guard the autonomous loop re-runs orchestration EVERY tick —
                    // taking false `advanced` credit, spending an action, and
                    // appending a Chronicle event each time — whenever the Mandate
                    // carries unassigned Briefs the orchestration skeleton does not
                    // own (e.g. Prime-decomposed child Briefs, which open unassigned
                    // and are NOT picked up by `assign_ready`). That is a livelock
                    // with misleading provenance. Real progress = a new Brief was
                    // created OR the unassigned count fell; the result body's
                    // `assigned_briefs` is NOT a reliable signal (it also reports an
                    // already-assigned subject as an idempotent re-assert). Compare
                    // the Mandate's brief shape before/after instead. A no-op run is
                    // `skipped` (no action consumed, no Chronicle event), so the
                    // Mandate parks honestly at the assignment gate, not spinning.
                    let after_ids: Vec<String> = task_store
                        .list_briefs_by_mandate(&mid, 500)
                        .map(|cards| cards.into_iter().map(|c| c.task_id).collect())
                        .unwrap_or_default();
                    let after = brief_counts(agent_store, task_store, tenant, &after_ids);
                    let progressed = after.total > step.counts.total
                        || after.unassigned < step.counts.unassigned;
                    let (outcome, reason) = if !progressed {
                        (
                            "skipped",
                            "orchestration is idempotent this tick — no Brief was \
                             created and no unassigned Brief was assigned; any \
                             remaining unassigned Briefs (e.g. Prime-decomposed \
                             children) await assignment"
                                .to_string(),
                        )
                    } else {
                        *actions += 1;
                        chronicle_autonomous(
                            task_store,
                            &mid,
                            "prime.autonomous_advance",
                            &format!(
                                "autonomous Prime advanced `{action}` on mandate {mid} (text: {})",
                                orch_mode.as_str()
                            ),
                        );
                        (
                            "advanced",
                            format!("ran governed `{action}` ({} text)", orch_mode.as_str()),
                        )
                    };
                    let mut rec = mk(phase, action, outcome, reason, Some(mid));
                    rec.orchestration_ai_mode = Some(orch_mode.as_str().to_string());
                    rec.orchestration_ai_reason = orch_reason;
                    rec
                }
                // Governance / store refusal — propagate honestly, take no credit.
                HandlerOutcome::Err(e) => mk(
                    phase,
                    action,
                    "blocked",
                    format!("orchestrate refused: {}", e.cause),
                    Some(mid),
                ),
            };
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
                        // A drafted strategy gets its own distinct event; every
                        // other safe advance shares `prime.autonomous_advance`.
                        // (A strategy is drafted BEFORE orchestration, so usually
                        // no Brief exists yet — `chronicle_autonomous` no-ops and
                        // the PrimeAutonomyRecord is the only trace, by design.)
                        let (event, detail) = if action == ADVANCE_PROPOSE_STRATEGY {
                            (
                                "prime.autonomous_strategy_proposed",
                                format!(
                                    "autonomous Prime drafted a strategy proposal for mandate {mid}"
                                ),
                            )
                        } else {
                            (
                                "prime.autonomous_advance",
                                format!("autonomous Prime advanced `{action}` on mandate {mid}"),
                            )
                        };
                        chronicle_autonomous(task_store, mid, event, &detail);
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

    // (B) ready_to_start — start ready work through the SAME guarded run
    // pipeline, gated by the autonomous budget hard-stop. An already-APPROVED
    // Prime proposal starts through the existing governed `prime.start` path
    // (`pid` Some); a BARE Mandate (no owning proposal) starts its ready
    // same-tenant Briefs itself through [`start_bare_mandate_ready`] — claims,
    // duplicate-run guard, adapter probe, durable ledger, board advancement and
    // Chronicle all go through the shared heartbeat machinery (no new run system,
    // stamped as an autonomous/heartbeat-trigger run, not dashboard `manual`).
    if step.phase == "ready_to_start" {
        // (B-approve) Prime Plan-Package Approval — Standing Authority v1. BEFORE
        // opening a new (duplicate) package and BEFORE any raw start, if this
        // candidate has an OPEN plan-package confirm that autonomous Prime ITSELF
        // authored AND the Board granted the `prime.plan_package.approve` standing
        // authority for this Guild, ACCEPT/materialize the package through the
        // EXISTING governed plan-confirm path + exactly-once decomposition ledger.
        // With no grant this is a no-op (returns `None`) and the (B0) active-planner
        // hold below keeps the pending package open exactly as before. Independent
        // of the authoring switch: approval authority is separate from authoring.
        // Prime-authored packages only — a human/other-actor package is never
        // auto-approved. Runs in every trigger mode so a pending package is an
        // actionable governance gate the moment the grant exists.
        if let Some(mid) = mandate_id.clone()
            && let Some(rec) = try_approve_prime_plan_package_for_mandate(
                agent_store,
                task_store,
                tenant,
                &mid,
                now_ms,
                actions,
                max,
                &phase,
                &mk,
            )
        {
            return rec;
        }
        // (B0) Active planner (Prime Active Planner Trigger v2 — `before_execute`).
        // BEFORE starting/executing this candidate's ready work, if the master
        // plan-package opt-in is on AND the trigger is `before_execute` AND the
        // Mandate has exactly one ELIGIBLE un-decomposed leaf Brief, open a
        // *proposed* decomposition plan package FIRST and HOLD the raw start,
        // leaving the confirm OPEN for a human. This is the ONLY preemption: it
        // fires only for a lone eligible leaf Brief at `ready_to_start` (a Mandate
        // with many Briefs, a terminal/childless-but-planned/locked Brief, or an
        // already-open package is `NotApplicable`/`Blocked` and is NOT preempted —
        // see [`plan_package_eligibility`]), and it NEVER touches the higher-priority
        // governance gates (proposal/strategy approval, team plan, hire/clearance),
        // which classify to DIFFERENT phases and never reach here. While a package
        // awaits approval the start stays HELD (the re-tick reports `skipped`,
        // consuming no budget) — Prime proposes a decomposition and waits for a
        // human instead of starting raw, undecomposed work. Reuses the SAME shared
        // open path as the (B5) tail. Never self-approves / assigns / creates
        // children.
        if plan_package_llm_enabled
            && plan_package_trigger == PrimePlanPackageTrigger::BeforeExecute
            && let Some(mid) = mandate_id.clone()
            && let Some(rec) = try_open_plan_package_for_mandate(
                task_store,
                spine_store,
                ai,
                tenant,
                &mid,
                plan_package_llm_enabled,
                plan_package_trigger,
                true, // preempting a raw start
                actions,
                max,
                &phase,
                &mk,
            )
        {
            return rec;
        }
        let Some(pid) = step.proposal_id.clone() else {
            let Some(mid) = mandate_id.clone() else {
                return mk(
                    phase,
                    "none",
                    "skipped",
                    "ready bare Mandate has no mandate id".into(),
                    None,
                );
            };
            if *actions >= max {
                return mk(
                    phase,
                    "start_mandate",
                    "skipped",
                    "tick action budget reached".into(),
                    mandate_id,
                );
            }
            let (outcome, reason, started) = start_bare_mandate_ready(
                agent_store,
                spine_store,
                task_store,
                registry,
                metrics,
                now_ms,
                tenant,
                &mid,
            );
            // Count exactly ONE tick action only when at least one run actually
            // started; a budget block / no-ready-run records honestly and acts on
            // nothing. Chronicle the Mandate-level start on its root Brief.
            if started > 0 {
                *actions += 1;
                chronicle_autonomous(
                    task_store,
                    &mid,
                    "prime.autonomous_mandate_start",
                    &format!("autonomous Prime started {started} ready Shift(s) for mandate {mid}"),
                );
            }
            return mk(phase, "start_mandate", outcome, reason, mandate_id);
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

    // (B3) needs_approval + PROPOSED strategy — STANDING-AUTHORITY strategy
    // approval. A proposed Mandate strategy is normally a human gate (left
    // `blocked`), but when the Board granted `prime.strategy.approve` for THIS
    // Guild the loop may approve it on the Board's behalf through the EXISTING
    // governed `mandate.strategy.approve` handler. That handler/store only flips
    // `proposed` → `approved` (its UPDATE is `WHERE status='proposed'`), so a
    // REJECTED or MISSING strategy is refused by the store and never re-proposed —
    // a human rejection stays final. Tenant-scoped (the grant is checked per the
    // candidate's own Guild), bounded (skips when the tick action budget is spent,
    // consuming nothing), idempotent (once approved the next step is no longer
    // `needs_approval`, so a re-tick neither re-approves nor double-consumes), and
    // it consumes exactly ONE call of the bounded grant on success.
    if step.phase == "needs_approval"
        && step.action_api == "mandate.strategy.approve"
        && step.strategy_status.as_deref() == Some("proposed")
    {
        let now_secs = now_secs_from_ms(now_ms);
        if !standing_active(agent_store, tenant, CATEGORY_STRATEGY_APPROVE, now_secs) {
            return mk(
                phase,
                "none",
                "blocked",
                "proposed strategy — no prime.strategy.approve standing authority for this Guild"
                    .into(),
                mandate_id,
            );
        }
        if *actions >= max {
            return mk(
                phase,
                "approve_strategy",
                "skipped",
                "tick action budget reached".into(),
                mandate_id,
            );
        }
        let Some(mid) = mandate_id.clone() else {
            return mk(
                phase,
                "none",
                "skipped",
                "strategy approve: next step has no mandate".into(),
                None,
            );
        };
        // Route through the EXISTING governed strategy-approve handler (no new
        // handler, no SpineStore bypass). Its arg is the bare mandate id.
        let approve_ctx = autonomous_prime_ctx(tenant, mid.clone().into_bytes());
        return match handle_strategy_approve(spine_store, &approve_ctx) {
            HandlerOutcome::Ok(_) => {
                *actions += 1;
                let _ = consume_standing(agent_store, tenant, CATEGORY_STRATEGY_APPROVE, now_secs);
                chronicle_autonomous(
                    task_store,
                    &mid,
                    "prime.autonomous_strategy_approve",
                    &format!(
                        "autonomous Prime approved the proposed strategy for mandate {mid} via standing authority"
                    ),
                );
                mk(
                    phase,
                    "approve_strategy",
                    "advanced",
                    format!(
                        "approved the proposed strategy for mandate {mid} via prime.strategy.approve standing authority"
                    ),
                    Some(mid),
                )
            }
            // Governance / store refusal (e.g. a strategy that is no longer
            // proposed) — propagate honestly, take no credit, consume nothing.
            HandlerOutcome::Err(e) => mk(
                phase,
                "approve_strategy",
                "blocked",
                format!("strategy approve refused: {}", e.cause),
                Some(mid),
            ),
        };
    }

    // (B4) needs_review / needs_apply — STANDING-AUTHORITY Shift disposition (Prime
    // Shift Disposition v1, company-model §12.6). A completed Shift awaiting review
    // acceptance, or an accepted run awaiting apply, is normally a human gate (left
    // `blocked`), but when the Board granted the matching SEPARATE standing
    // authority for THIS Guild the loop may close it on the Board's behalf through
    // the EXISTING review/apply paths. Review (`prime.run.review_accept`) and apply
    // (`prime.run.apply`) are DISTINCT grants and DISTINCT ticks — a single tick
    // accepts XOR applies the one classified run (the classifier surfaces apply only
    // once a run is accepted, so the same run is never both in one tick). The run id
    // is re-validated at execution time (the review path rejects a non-`done` run;
    // `execute_run_apply` re-runs `run_apply_eligibility` + conflict/baseline checks
    // and only a clean `applied` advances the Brief), so a stale classification is
    // refused honestly with no credit taken. Tenant-scoped, bounded by the action
    // budget, and consumes exactly ONE call of the bounded grant on success.
    if step.phase == PHASE_NEEDS_REVIEW || step.phase == PHASE_NEEDS_APPLY {
        let now_secs = now_secs_from_ms(now_ms);
        let Some(run_id) = step.run_id.clone() else {
            return mk(
                phase,
                "none",
                "skipped",
                "disposition step carries no run id".into(),
                mandate_id,
            );
        };

        if step.phase == PHASE_NEEDS_REVIEW {
            if !standing_active(agent_store, tenant, CATEGORY_RUN_REVIEW_ACCEPT, now_secs) {
                return mk(
                    phase,
                    "none",
                    "blocked",
                    "completed Shift awaiting review — no prime.run.review_accept standing authority for this Guild".into(),
                    mandate_id,
                );
            }
            if *actions >= max {
                return mk(
                    phase,
                    ACTION_REVIEW_ACCEPT,
                    "skipped",
                    "tick action budget reached".into(),
                    mandate_id,
                );
            }
            // Route through the EXISTING review path (`set_run_review`) — the same
            // store call the manual `run.review` accept makes (which records the
            // `brief.run_reviewed` event). Acceptance does NOT apply or advance the
            // Brief; the apply tick (next) does, under its own separate grant.
            return match task_store.set_run_review(
                &run_id,
                "accepted",
                "accepted by autonomous Prime under standing authority",
            ) {
                Ok(_) => {
                    *actions += 1;
                    let _ =
                        consume_standing(agent_store, tenant, CATEGORY_RUN_REVIEW_ACCEPT, now_secs);
                    if let Some(mid) = mandate_id.as_deref() {
                        chronicle_autonomous(
                            task_store,
                            mid,
                            "prime.autonomous_review_accept",
                            &format!(
                                "autonomous Prime accepted review of run {run_id} on mandate {mid} via standing authority"
                            ),
                        );
                    }
                    mk(
                        phase,
                        ACTION_REVIEW_ACCEPT,
                        "advanced",
                        format!(
                            "accepted review of run {run_id} via prime.run.review_accept standing authority"
                        ),
                        mandate_id,
                    )
                }
                // The run is no longer `done` / reviewable — propagate honestly,
                // take no credit, consume nothing.
                Err(e) => mk(
                    phase,
                    ACTION_REVIEW_ACCEPT,
                    "blocked",
                    format!("review accept refused: {e}"),
                    mandate_id,
                ),
            };
        }

        // PHASE_NEEDS_APPLY — apply an already-accepted run through the EXISTING
        // safe apply machinery. NEVER a hand-rolled copy: `execute_run_apply`
        // re-runs `run_apply_eligibility`, the baseline-hash / conflict / artifact
        // safety checks, and (only on a clean `applied`) the review-to-done
        // `complete_reviewed_brief`. A conflicted/failed apply leaves the durable
        // apply state recorded and the Brief NOT done — we record `blocked` and do
        // NOT retry in this tick (the next tick's classifier excludes the now-
        // terminal apply run, so there is no blind retry loop).
        if !standing_active(agent_store, tenant, CATEGORY_RUN_APPLY, now_secs) {
            return mk(
                phase,
                "none",
                "blocked",
                "accepted Shift awaiting apply — no prime.run.apply standing authority for this Guild".into(),
                mandate_id,
            );
        }
        if *actions >= max {
            return mk(
                phase,
                ACTION_APPLY_RUN,
                "skipped",
                "tick action budget reached".into(),
                mandate_id,
            );
        }
        return match crate::controller_runtime::execute_run_apply(task_store, &run_id, tenant) {
            Ok(v) => {
                let apply_status = v
                    .get("apply_status")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if apply_status == "applied" {
                    *actions += 1;
                    let _ = consume_standing(agent_store, tenant, CATEGORY_RUN_APPLY, now_secs);
                    if let Some(mid) = mandate_id.as_deref() {
                        chronicle_autonomous(
                            task_store,
                            mid,
                            "prime.autonomous_apply",
                            &format!(
                                "autonomous Prime applied run {run_id} on mandate {mid} via standing authority"
                            ),
                        );
                    }
                    mk(
                        phase,
                        ACTION_APPLY_RUN,
                        "advanced",
                        format!("applied run {run_id} via prime.run.apply standing authority"),
                        mandate_id,
                    )
                } else {
                    // conflicted / failed — apply machinery already recorded the
                    // durable apply state; the Brief is NOT done. Block, no retry,
                    // no grant consumed.
                    mk(
                        phase,
                        ACTION_APPLY_RUN,
                        "blocked",
                        format!(
                            "apply did not complete (apply_status={apply_status}) — left for human"
                        ),
                        mandate_id,
                    )
                }
            }
            // Eligibility / store refusal — propagate honestly, take no credit.
            Err(e) => mk(
                phase,
                ACTION_APPLY_RUN,
                "blocked",
                format!("apply refused: {e}"),
                mandate_id,
            ),
        };
    }

    // (B5) Prime Plan-Package Authoring — tail gap-fill (opt-in, default OFF via
    // `RELIX_PRIME_LLM_PLAN_PACKAGE`). For a candidate the existing governed flow
    // leaves IDLE (we reached here without a safe advance / start / governance /
    // disposition action), Prime may author a *proposed* decomposition plan
    // package on a single un-decomposed Brief and LEAVE the confirm OPEN for a
    // human, through the SHARED [`try_open_plan_package_for_mandate`] open path
    // (the EXISTING `open_plan_package` primitive). This runs in EVERY trigger mode
    // as the catch-all: in `before_execute` the (B0) step above already preempts a
    // raw start for a ready leaf Brief, so the only candidates reaching here are
    // genuinely idle (blocked / not yet startable). It is purely additive: with the
    // flag off the tick authors nothing (byte-for-byte legacy), and it NEVER
    // competes with orchestrate/start (those returned earlier). It NEVER approves
    // the confirm itself. Bounded by the action budget, tenant-scoped, idempotent /
    // dedup-guarded (no second package, never over a human/locked/existing plan).
    // Because the plan-package primitive attaches to a Brief, the candidate's
    // resolved Mandate must carry one.
    //
    // (B5-approve) First, the standing-authority approval gate (Plan-Package
    // Approval v1): a tail/idle candidate may already carry an OPEN plan-package
    // confirm Prime itself authored on a PRIOR tick. If the Guild granted
    // `prime.plan_package.approve`, ACCEPT/materialize it through the existing
    // plan-confirm path BEFORE the (B5) open path would merely re-report it as
    // pending. With no grant this is a no-op and (B5) reports the pending package
    // exactly as before. Prime-authored packages only.
    if let Some(mid) = mandate_id.clone()
        && let Some(rec) = try_approve_prime_plan_package_for_mandate(
            agent_store,
            task_store,
            tenant,
            &mid,
            now_ms,
            actions,
            max,
            &phase,
            &mk,
        )
    {
        return rec;
    }
    if plan_package_llm_enabled
        && let Some(mid) = mandate_id.clone()
        && let Some(rec) = try_open_plan_package_for_mandate(
            task_store,
            spine_store,
            ai,
            tenant,
            &mid,
            plan_package_llm_enabled,
            plan_package_trigger,
            false, // idle tail — report any block honestly, never preempt
            actions,
            max,
            &phase,
            &mk,
        )
    {
        return rec;
    }

    // (C) Everything else needs a human gate, or is already running / done —
    // record it, act on nothing, write no event.
    let outcome = match step.phase {
        "needs_approval" | "needs_hire_approval" | "blocked" => "blocked",
        _ => "skipped",
    };
    mk(phase, "none", outcome, step.reason.clone(), mandate_id)
}

/// Parse `RELIX_PRIME_LLM_PRIORITIZATION` (`1|true|yes|on`, case-insensitive) into
/// the model-prioritization flag (Prime Executive Prioritization v1). Default OFF.
/// Independent of the deliberation / strategy-draft switches: a Guild may let the
/// model ORDER the legal candidate queue while keeping deterministic per-candidate
/// action choice + strategy authoring (or any combination). It never widens the
/// menu — only the already-legal, already-attemptable candidates are reordered.
pub fn parse_prime_llm_prioritization(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// One discovered autonomous candidate, in deterministic discovery order (the
/// prioritization FALLBACK order). Carries just enough to (a) classify it
/// read-only for the prioritization menu and (b) execute exactly the governed step
/// it would run today.
enum AutoCandidate {
    /// PASS 0 — a PROPOSED Prime proposal whose Guild granted the
    /// `prime.proposal.approve` standing authority (already filtered).
    ProposalApprove(crate::nodes::coordinator::spine::store::PrimeProposalRow),
    /// An approved Prime proposal → `process_candidate("proposal")`.
    Proposal { tenant: String, proposal_id: String },
    /// A bare active Mandate (no owning proposal) → `process_candidate("mandate")`.
    Mandate { tenant: String, mandate_id: String },
}

impl AutoCandidate {
    /// The record `target_kind` (`proposal` / `mandate`).
    fn kind(&self) -> &'static str {
        match self {
            AutoCandidate::ProposalApprove(_) | AutoCandidate::Proposal { .. } => "proposal",
            AutoCandidate::Mandate { .. } => "mandate",
        }
    }
}

/// The ONE governed action the deterministic classifier would actually ATTEMPT
/// for a classified step THIS tick — the gate for offering a candidate to the
/// prioritization model. Differs from [`intended_action`] in that an approval-
/// category action (hire / clearance / strategy) is only "attemptable" when the
/// Guild holds the matching live standing authority (and, for hire/clearance, a
/// known hire Rig); otherwise it is a pure human gate this tick and is NOT
/// offered. `None` for a human gate / running / done step the loop cannot action.
fn attemptable_action(
    step: &NextStep,
    agent_store: &AgentStore,
    tenant: &str,
    now_secs: i64,
    hire_rig: &str,
) -> Option<&'static str> {
    if step.can_advance {
        return step.advance_action;
    }
    match intended_action(step) {
        Some(a @ ("start" | "start_mandate")) => Some(a),
        Some("hire_approve") => {
            (standing_active(agent_store, tenant, CATEGORY_HIRE_APPROVE, now_secs)
                && crate::rig::is_known_rig(hire_rig))
            .then_some("hire_approve")
        }
        Some("clearance_approve") => {
            (standing_active(agent_store, tenant, CATEGORY_CLEARANCE_APPROVE, now_secs)
                && crate::rig::is_known_rig(hire_rig))
            .then_some("clearance_approve")
        }
        Some("approve_strategy") => {
            standing_active(agent_store, tenant, CATEGORY_STRATEGY_APPROVE, now_secs)
                .then_some("approve_strategy")
        }
        Some(a @ "review_accept") => {
            standing_active(agent_store, tenant, CATEGORY_RUN_REVIEW_ACCEPT, now_secs).then_some(a)
        }
        Some(a @ "apply_run") => {
            standing_active(agent_store, tenant, CATEGORY_RUN_APPLY, now_secs).then_some(a)
        }
        _ => None,
    }
}

/// Build the bounded, secret-free prioritization-menu summary for a Proposal /
/// Mandate candidate, or `None` when its next governed step is NOT an attemptable
/// action this tick (a pure human gate / running / done). READ-ONLY.
#[allow(clippy::too_many_arguments)]
fn menu_from_target(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &Arc<TaskStore>,
    now_secs: i64,
    hire_rig: &str,
    key: &str,
    tenant: &str,
    kind: &str,
    target_id: &str,
    target: Value,
) -> Option<PrimePriorityCandidate> {
    let read_ctx = autonomous_prime_ctx(tenant, target.to_string().into_bytes());
    let step = compute_next_step(agent_store, spine_store, task_store, &read_ctx).ok()?;
    let action = attemptable_action(&step, agent_store, tenant, now_secs, hire_rig)?;
    Some(PrimePriorityCandidate {
        key: key.to_string(),
        tenant: tenant.to_string(),
        target_kind: kind.to_string(),
        target_id: target_id.to_string(),
        mandate_id: step.mandate_id.clone(),
        phase: step.phase.to_string(),
        computed_action: action.to_string(),
        reason: step.reason.clone(),
        strategy_status: step.strategy_status.clone(),
        total_briefs: step.counts.total,
        ready: step.counts.ready,
        unassigned: step.counts.unassigned,
        running: step.counts.running,
        needs_review: step.counts.needs_review,
        blocked: step.counts.blocked,
        missing_roles: step.missing_roles.len(),
        pending_hires: step.pending_hires.len(),
        pending_clearances: step.pending_clearances.len(),
    })
}

/// The prioritization-menu summary for a discovered candidate, or `None` when it
/// has no attemptable action this tick. A PASS-0 `ProposalApprove` is always
/// attemptable (it was discovered only with a live `prime.proposal.approve`
/// grant). READ-ONLY.
fn candidate_menu_entry(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &Arc<TaskStore>,
    now_secs: i64,
    hire_rig: &str,
    key: &str,
    cand: &AutoCandidate,
) -> Option<PrimePriorityCandidate> {
    match cand {
        AutoCandidate::ProposalApprove(p) => Some(PrimePriorityCandidate {
            key: key.to_string(),
            tenant: p.tenant_id.clone(),
            target_kind: "proposal".to_string(),
            target_id: p.proposal_id.clone(),
            mandate_id: None,
            phase: "needs_approval".to_string(),
            computed_action: "approve".to_string(),
            reason: "approve the proposed plan via the prime.proposal.approve standing authority"
                .to_string(),
            strategy_status: None,
            total_briefs: 0,
            ready: 0,
            unassigned: 0,
            running: 0,
            needs_review: 0,
            blocked: 0,
            missing_roles: 0,
            pending_hires: 0,
            pending_clearances: 0,
        }),
        AutoCandidate::Proposal {
            tenant,
            proposal_id,
        } => menu_from_target(
            agent_store,
            spine_store,
            task_store,
            now_secs,
            hire_rig,
            key,
            tenant,
            "proposal",
            proposal_id,
            json!({ "proposal_id": proposal_id }),
        ),
        AutoCandidate::Mandate { tenant, mandate_id } => menu_from_target(
            agent_store,
            spine_store,
            task_store,
            now_secs,
            hire_rig,
            key,
            tenant,
            "mandate",
            mandate_id,
            json!({ "mandate_id": mandate_id }),
        ),
    }
}

/// Execute the PASS-0 standing-authority approval of a PROPOSED Prime proposal —
/// the existing `prime.approve` path, optionally pre-confirmed by the Prime
/// Deliberation layer. Bounded by a self-guard on the action budget so it stays
/// safe in the prioritized (non-break) execution order. Returns the record with
/// the deliberation provenance stamped; the caller stamps the prioritization
/// provenance.
#[allow(clippy::too_many_arguments)]
fn exec_proposal_approve(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &Arc<TaskStore>,
    now_secs: i64,
    p: &crate::nodes::coordinator::spine::store::PrimeProposalRow,
    actions: &mut usize,
    max: usize,
    ai: Option<&dyn PrimeAiDecider>,
    llm_enabled: bool,
) -> PrimeAutonomyRecord {
    let base = |outcome: &'static str,
                reason: String,
                mandate_id: Option<String>,
                ai_mode: Option<String>,
                ai_reason: Option<String>|
     -> PrimeAutonomyRecord {
        PrimeAutonomyRecord {
            tenant: p.tenant_id.clone(),
            target_kind: "proposal",
            target_id: p.proposal_id.clone(),
            mandate_id,
            phase: "needs_approval".to_string(),
            action: "approve",
            outcome,
            reason,
            ai_mode,
            ai_reason,
            strategy_ai_mode: None,
            strategy_ai_reason: None,
            priority_ai_mode: None,
            priority_ai_reason: None,
            priority_rank: None,
            orchestration_ai_mode: None,
            orchestration_ai_reason: None,
            plan_package_ai_mode: None,
            plan_package_ai_reason: None,
            plan_doc_id: None,
            suggestion_id: None,
            confirm_id: None,
            child_count: None,
            plan_package_trigger: None,
        }
    };

    // Budget guard — in the deterministic order the caller breaks first, but the
    // prioritized order relies on this self-guard to stay bounded by `max`.
    if *actions >= max {
        return base(
            "skipped",
            "tick action budget reached".to_string(),
            None,
            None,
            None,
        );
    }

    // Prime Deliberation v1: let the model CONFIRM this autonomous approval or HOLD
    // (`none`) this tick. The standing grant is still required and is consumed only
    // on a real approval below; a HOLD records `skipped` and consumes nothing.
    let mut pass0_mode = PrimeDeliberationMode::DeterministicOnly;
    let mut pass0_reason: Option<String> = None;
    if llm_enabled {
        let snap = PrimeDeliberationInput {
            tenant: p.tenant_id.clone(),
            target_kind: "proposal".to_string(),
            target_id: p.proposal_id.clone(),
            mandate_id: None,
            phase: "needs_approval".to_string(),
            computed_action: "approve".to_string(),
            reason: "approve the proposed plan via the prime.proposal.approve standing authority"
                .to_string(),
            strategy_status: None,
            total_briefs: 0,
            ready: 0,
            unassigned: 0,
            running: 0,
            needs_review: 0,
            blocked: 0,
            missing_roles: 0,
            pending_hires: 0,
            pending_clearances: 0,
        };
        match deliberate(ai, &snap) {
            Deliberation::Hold { mode, reason } => {
                return base(
                    "skipped",
                    match &reason {
                        Some(r) => format!("model chose to hold (none): {r}"),
                        None => "model chose to hold (none)".to_string(),
                    },
                    None,
                    Some(mode.as_str().to_string()),
                    reason,
                );
            }
            Deliberation::Proceed { mode, reason } => {
                pass0_mode = mode;
                pass0_reason = reason;
            }
        }
    }

    let approve_ctx = autonomous_prime_ctx(&p.tenant_id, p.proposal_id.clone().into_bytes());
    let mut rec = match handle_prime_approve(agent_store, spine_store, task_store, &approve_ctx) {
        HandlerOutcome::Ok(b) => {
            *actions += 1;
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
            base(
                "approved",
                "materialized proposed plan through the existing prime.approve path".to_string(),
                mid,
                None,
                None,
            )
        }
        HandlerOutcome::Err(e) => base(
            "blocked",
            format!("autonomous approve refused: {}", e.cause),
            None,
            None,
            None,
        ),
    };
    rec.ai_mode = Some(pass0_mode.as_str().to_string());
    rec.ai_reason = pass0_reason;
    rec
}

/// Dispatch ONE discovered candidate to its governed executor (the SAME governed
/// handlers + gates the manual route uses). PASS-0 approvals run `prime.approve`;
/// every other candidate runs through [`process_candidate`] (which itself carries
/// the per-candidate deliberation + strategy-authoring layers). Stamps the
/// deliberation/strategy provenance; the caller stamps the prioritization
/// provenance.
#[allow(clippy::too_many_arguments)]
fn execute_candidate(
    agent_store: &AgentStore,
    spine_store: &SpineStore,
    task_store: &Arc<TaskStore>,
    registry: &crate::rig::RigRegistry,
    metrics: Option<&crate::metrics::MetricsQuery>,
    now_ms: i64,
    now_secs: i64,
    hire_rig: &str,
    cand: &AutoCandidate,
    actions: &mut usize,
    max: usize,
    ai: Option<&dyn PrimeAiDecider>,
    llm_enabled: bool,
    strategy_llm_enabled: bool,
    orchestration_llm_enabled: bool,
    plan_package_llm_enabled: bool,
    plan_package_trigger: PrimePlanPackageTrigger,
) -> PrimeAutonomyRecord {
    match cand {
        AutoCandidate::ProposalApprove(p) => exec_proposal_approve(
            agent_store,
            spine_store,
            task_store,
            now_secs,
            p,
            actions,
            max,
            ai,
            llm_enabled,
        ),
        AutoCandidate::Proposal {
            tenant,
            proposal_id,
        } => process_candidate(
            agent_store,
            spine_store,
            task_store,
            registry,
            metrics,
            now_ms,
            tenant,
            "proposal",
            proposal_id,
            json!({ "proposal_id": proposal_id }),
            actions,
            max,
            hire_rig,
            ai,
            llm_enabled,
            strategy_llm_enabled,
            orchestration_llm_enabled,
            plan_package_llm_enabled,
            plan_package_trigger,
        ),
        AutoCandidate::Mandate { tenant, mandate_id } => process_candidate(
            agent_store,
            spine_store,
            task_store,
            registry,
            metrics,
            now_ms,
            tenant,
            "mandate",
            mandate_id,
            json!({ "mandate_id": mandate_id }),
            actions,
            max,
            hire_rig,
            ai,
            llm_enabled,
            strategy_llm_enabled,
            orchestration_llm_enabled,
            plan_package_llm_enabled,
            plan_package_trigger,
        ),
    }
}

/// Run ONE opt-in autonomous Prime tick: discover up to a bounded set of
/// candidates (standing-approvable PROPOSED proposals first, then approved Prime
/// proposals — they carry Start — then live Mandates not already covered by a
/// proposal) and apply at most `max` safe governed actions across them, returning
/// one [`PrimeAutonomyRecord`] per candidate considered. Pure of any sleeping /
/// timer — the controller calls it on an interval inside `spawn_blocking`.
///
/// Prime Executive Prioritization v1: when `prioritization_enabled` and a live
/// decider is wired AND there are ≥2 *attemptable* candidates, an opt-in model is
/// asked ONLY to ORDER the already-legal candidate menu (or HOLD the whole queue
/// this tick). THE MODEL IS NOT THE PERMISSION SYSTEM — it can never invent a
/// candidate, add an action to the menu, widen a candidate's action, or bypass any
/// standing-authority / budget / claim / adapter / tenant gate; only the
/// deterministic classifier's already-attemptable candidates are offered, and any
/// malformed / out-of-set / unavailable output degrades to the deterministic
/// discovery order with an honest mode. With prioritization off (or <2
/// candidates) the discovery order is byte-for-byte the legacy behaviour.
///
/// Tenant-safe: `tenant=None` spans **all** Guilds (each candidate carries its
/// own `tenant_id` and is processed under it); `tenant=Some(g)` scopes to one
/// Guild. Idempotent: each tick re-classifies live state, so a team plan /
/// orchestration tree / started Shift is never duplicated and an already-
/// running Brief is never double-started. Bounded: `max` caps actions per tick.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
    ai: Option<&dyn PrimeAiDecider>,
    llm_enabled: bool,
    strategy_llm_enabled: bool,
    prioritization_enabled: bool,
    orchestration_llm_enabled: bool,
    plan_package_llm_enabled: bool,
    plan_package_trigger: PrimePlanPackageTrigger,
) -> Result<Vec<PrimeAutonomyRecord>, String> {
    if max == 0 {
        return Ok(Vec::new());
    }
    // Bounded discovery — never an unbounded table scan.
    let discover_cap = max.saturating_mul(4).clamp(max, 50);
    let now_secs = now_secs_from_ms(now_ms);

    // ── DISCOVER — build the deterministic candidate queue (the prioritization
    // FALLBACK order). PASS-0 proposals are filtered to a live
    // `prime.proposal.approve` grant here, so an unauthorized proposal is left
    // proposed silently (never queued, never recorded) — exactly the legacy skip.
    let mut queue: Vec<AutoCandidate> = Vec::new();
    let proposed = spine_store
        .list_proposed_prime_proposals(tenant, discover_cap)
        .map_err(|e| format!("autonomous prime: list proposed: {e}"))?;
    for p in proposed {
        if standing_active(
            agent_store,
            &p.tenant_id,
            CATEGORY_PROPOSAL_APPROVE,
            now_secs,
        ) {
            queue.push(AutoCandidate::ProposalApprove(p));
        }
    }
    let proposals = spine_store
        .list_approved_prime_proposals(tenant, discover_cap)
        .map_err(|e| format!("autonomous prime: list proposals: {e}"))?;
    // Mandate ids already covered by a proposal — so the bare-Mandate pass does
    // not double-process the same Mandate.
    let mut seen_mandates: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in proposals {
        if !p.mandate_id.is_empty() {
            seen_mandates.insert(p.mandate_id.clone());
        }
        queue.push(AutoCandidate::Proposal {
            tenant: p.tenant_id,
            proposal_id: p.proposal_id,
        });
    }
    let mandates = spine_store
        .list_active_mandates(tenant, discover_cap)
        .map_err(|e| format!("autonomous prime: list mandates: {e}"))?;
    for m in mandates {
        if seen_mandates.contains(&m.mandate_id) {
            continue;
        }
        queue.push(AutoCandidate::Mandate {
            tenant: m.tenant_id,
            mandate_id: m.mandate_id,
        });
    }

    // Stable, opaque per-tick keys the prioritization model may reorder.
    let keys: Vec<String> = (0..queue.len())
        .map(|i| format!("cand-{}", i + 1))
        .collect();

    // ── CLASSIFY (READ-ONLY) — the attemptable-action menu: candidates whose next
    // governed step is a positive action the loop could actually take this tick.
    // Pure human-gate / running / done candidates are NOT offered to the model
    // (they are still recorded deterministically below). Bounded.
    let mut menu: Vec<(usize, PrimePriorityCandidate)> = Vec::new();
    for (i, cand) in queue.iter().enumerate() {
        if menu.len() >= MAX_PRIORITY_CANDIDATES {
            break;
        }
        if let Some(entry) = candidate_menu_entry(
            agent_store,
            spine_store,
            task_store,
            now_secs,
            hire_rig,
            &keys[i],
            cand,
        ) {
            menu.push((i, entry));
        }
    }

    // ── DECIDE the execution order. The model may ONLY reorder the already-legal
    // menu (or hold the whole queue); anything else degrades to the deterministic
    // discovery order with an honest mode. Only consulted with ≥2 attemptable
    // candidates — there is nothing to order otherwise.
    let mut priority_mode = PrimePriorityMode::DeterministicOnly;
    let mut priority_reason: Option<String> = None;
    let mut ranked: Vec<usize> = menu.iter().map(|(i, _)| *i).collect();
    let mut hold = false;

    if prioritization_enabled && menu.len() >= 2 {
        match ai {
            None => {
                priority_mode = PrimePriorityMode::Unavailable;
                priority_reason = Some("no AI decider wired for prioritization".to_string());
            }
            Some(decider) => {
                let offered: Vec<PrimePriorityCandidate> =
                    menu.iter().map(|(_, c)| c.clone()).collect();
                let offered_keys: Vec<String> = offered.iter().map(|c| c.key.clone()).collect();
                let prompt = build_priority_prompt(&offered);
                match decider.deliberate(&prompt) {
                    Ok(raw) => match parse_priority_order(&raw, &offered_keys) {
                        Ok(order) => {
                            priority_mode = PrimePriorityMode::LlmUsed;
                            priority_reason = non_empty(order.reason);
                            if order.order.is_empty() {
                                hold = true;
                            } else {
                                let key_to_index: std::collections::HashMap<&str, usize> =
                                    menu.iter().map(|(i, c)| (c.key.as_str(), *i)).collect();
                                let listed: std::collections::HashSet<&str> =
                                    order.order.iter().map(String::as_str).collect();
                                let mut new_ranked: Vec<usize> = Vec::with_capacity(menu.len());
                                for k in &order.order {
                                    if let Some(&idx) = key_to_index.get(k.as_str()) {
                                        new_ranked.push(idx);
                                    }
                                }
                                // Append any un-listed menu candidate (the model
                                // deprioritized it, not dropped it) in det. order.
                                for (i, c) in &menu {
                                    if !listed.contains(c.key.as_str()) {
                                        new_ranked.push(*i);
                                    }
                                }
                                ranked = new_ranked;
                            }
                        }
                        Err(e) => {
                            priority_mode = PrimePriorityMode::Fallback;
                            priority_reason = Some(format!("model priority output rejected: {e}"));
                        }
                    },
                    Err(e) => {
                        priority_mode = PrimePriorityMode::Unavailable;
                        priority_reason = Some(format!("model unavailable: {e}"));
                    }
                }
            }
        }
    }

    let priority_mode_str = priority_mode.as_str().to_string();
    let mut records: Vec<PrimeAutonomyRecord> = Vec::new();
    let mut actions = 0usize;

    // ── HOLD — the model declined to act on any candidate this tick. Record every
    // offered candidate as held (ZERO side effects); still record the non-offered
    // human-gate candidates deterministically (read-only).
    if hold {
        let menu_idx: std::collections::HashSet<usize> = menu.iter().map(|(i, _)| *i).collect();
        for (rank0, (i, c)) in menu.iter().enumerate() {
            records.push(PrimeAutonomyRecord {
                tenant: c.tenant.clone(),
                target_kind: queue[*i].kind(),
                target_id: c.target_id.clone(),
                mandate_id: c.mandate_id.clone(),
                phase: c.phase.clone(),
                action: "none",
                outcome: "skipped",
                reason: match &priority_reason {
                    Some(r) => format!("model chose to hold the queue: {r}"),
                    None => "model chose to hold the queue".to_string(),
                },
                ai_mode: None,
                ai_reason: None,
                strategy_ai_mode: None,
                strategy_ai_reason: None,
                priority_ai_mode: Some(priority_mode_str.clone()),
                priority_ai_reason: priority_reason.clone(),
                priority_rank: Some(rank0 + 1),
                orchestration_ai_mode: None,
                orchestration_ai_reason: None,
                plan_package_ai_mode: None,
                plan_package_ai_reason: None,
                plan_doc_id: None,
                suggestion_id: None,
                confirm_id: None,
                child_count: None,
                plan_package_trigger: None,
            });
        }
        for (i, cand) in queue.iter().enumerate() {
            if menu_idx.contains(&i) {
                continue;
            }
            let mut rec = execute_candidate(
                agent_store,
                spine_store,
                task_store,
                registry,
                metrics,
                now_ms,
                now_secs,
                hire_rig,
                cand,
                &mut actions,
                max,
                ai,
                llm_enabled,
                strategy_llm_enabled,
                orchestration_llm_enabled,
                plan_package_llm_enabled,
                plan_package_trigger,
            );
            rec.priority_ai_mode = Some(priority_mode_str.clone());
            rec.priority_ai_reason = priority_reason.clone();
            records.push(rec);
        }
        return Ok(records);
    }

    // ── EXECUTE in the model-picked order (only when the model actually picked
    // one). Run the ranked menu first — each candidate self-guards on the action
    // budget, so candidates beyond `max` record `skipped` with their rank — then
    // the remaining (non-menu / over-cap) candidates in deterministic order.
    if priority_mode == PrimePriorityMode::LlmUsed {
        let rank_of: std::collections::HashMap<usize, usize> = ranked
            .iter()
            .enumerate()
            .map(|(r, &i)| (i, r + 1))
            .collect();
        let in_ranked: std::collections::HashSet<usize> = ranked.iter().copied().collect();
        let mut exec_order: Vec<usize> = ranked.clone();
        for i in 0..queue.len() {
            if !in_ranked.contains(&i) {
                exec_order.push(i);
            }
        }
        for &i in &exec_order {
            let mut rec = execute_candidate(
                agent_store,
                spine_store,
                task_store,
                registry,
                metrics,
                now_ms,
                now_secs,
                hire_rig,
                &queue[i],
                &mut actions,
                max,
                ai,
                llm_enabled,
                strategy_llm_enabled,
                orchestration_llm_enabled,
                plan_package_llm_enabled,
                plan_package_trigger,
            );
            rec.priority_ai_mode = Some(priority_mode_str.clone());
            rec.priority_ai_reason = priority_reason.clone();
            rec.priority_rank = rank_of.get(&i).copied();
            records.push(rec);
        }
        return Ok(records);
    }

    // ── DETERMINISTIC ORDER (off / fallback / unavailable / <2 candidates):
    // byte-for-byte the legacy discovery order with break-on-budget, only stamped
    // with the prioritization provenance.
    for cand in &queue {
        if actions >= max {
            break;
        }
        let mut rec = execute_candidate(
            agent_store,
            spine_store,
            task_store,
            registry,
            metrics,
            now_ms,
            now_secs,
            hire_rig,
            cand,
            &mut actions,
            max,
            ai,
            llm_enabled,
            strategy_llm_enabled,
            orchestration_llm_enabled,
            plan_package_llm_enabled,
            plan_package_trigger,
        );
        rec.priority_ai_mode = Some(priority_mode_str.clone());
        rec.priority_ai_reason = priority_reason.clone();
        records.push(rec);
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

    // ── PRIME STRATEGY DRAFTING v1 (manual prime.advance propose_strategy) ──

    /// A bare Mandate (no strategy yet).
    fn bare_mandate(spine: &SpineStore) -> String {
        spine
            .create_mandate(
                "default",
                "Ship the login page",
                "wire it to auth",
                None,
                None,
            )
            .unwrap()
    }

    // S1) A Mandate with NO strategy → next step is `needs_strategy_proposal` and a
    //     manual `prime.advance propose_strategy` drafts + proposes a strategy that
    //     lands `proposed` (NOT approved). Idempotent: a re-advance refuses as stale.
    #[test]
    fn no_strategy_advances_propose_strategy_to_proposed_not_approved() {
        let (agents, spine, tasks) = stores();
        let m = bare_mandate(&spine);

        let v = next_step(&agents, &spine, &tasks, json!({ "mandate_id": m }));
        assert_eq!(v["phase"], "needs_strategy_proposal");
        assert_eq!(v["can_advance"], true);
        assert_eq!(v["advance_action"], "propose_strategy");
        assert!(v["strategy_status"].is_null());

        let r = advance(
            &agents,
            &spine,
            &tasks,
            json!({ "mandate_id": m, "action": "propose_strategy" }),
        );
        assert_eq!(r["advanced"], true);
        assert_eq!(r["action"], "propose_strategy");
        // The strategy is now proposed but NOT approved (drafting is not approval).
        assert_eq!(
            spine.strategy_status("default", &m).unwrap().as_deref(),
            Some("proposed")
        );
        assert!(!spine.strategy_approved("default", &m).unwrap());
        // The next step has moved to the (human) strategy-approval gate.
        let after = &r["next_step"];
        assert_eq!(after["phase"], "needs_approval");
        assert_eq!(after["action_api"], "mandate.strategy.approve");
        assert_eq!(after["can_advance"], false);

        // A second propose_strategy now refuses as stale — no overwrite.
        let again = advance(
            &agents,
            &spine,
            &tasks,
            json!({ "mandate_id": m, "action": "propose_strategy" }),
        );
        assert_eq!(again["advanced"], false);
        assert_eq!(again["refused"], "stale_action");
        assert_eq!(
            spine.strategy_status("default", &m).unwrap().as_deref(),
            Some("proposed")
        );
    }

    // S2) A rejected strategy is NOT overwritten by propose_strategy (Prime does
    //     not fight a human rejection): the step is `blocked` and advance is stale.
    #[test]
    fn rejected_strategy_is_not_overwritten() {
        let (agents, spine, tasks) = stores();
        let m = bare_mandate(&spine);
        spine
            .propose_strategy("default", &m, "first draft")
            .unwrap();
        spine.reject_strategy("default", &m).unwrap();

        let v = next_step(&agents, &spine, &tasks, json!({ "mandate_id": m }));
        assert_eq!(v["phase"], "blocked");
        assert_eq!(v["can_advance"], false);

        let r = advance(
            &agents,
            &spine,
            &tasks,
            json!({ "mandate_id": m, "action": "propose_strategy" }),
        );
        assert_eq!(r["advanced"], false);
        assert_eq!(r["refused"], "stale_action");
        // Still rejected — never reset to proposed.
        assert_eq!(
            spine.strategy_status("default", &m).unwrap().as_deref(),
            Some("rejected")
        );
    }

    // S3) The deterministic draft is non-empty, useful, and pipe-safe.
    #[test]
    fn draft_mandate_strategy_is_useful_and_pipe_safe() {
        let spine = SpineStore::in_memory().unwrap();
        let m = spine
            .create_mandate(
                "default",
                "Build | the thing",
                "a | piped | description",
                None,
                None,
            )
            .unwrap();
        let mandate = spine
            .get_mandate_for_tenant(&m, "default")
            .unwrap()
            .unwrap();
        let doc = draft_mandate_strategy(&mandate, &["engineer", "designer"]);
        assert!(
            !doc.contains('|'),
            "pipe must be sanitized out of the draft"
        );
        assert!(doc.contains("DRAFT"), "draft must say it is not approved");
        assert!(doc.contains("Objective"));
        assert!(doc.contains("engineer"));
        assert!(doc.chars().count() <= STRATEGY_DRAFT_BODY_CAP);
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
        autonomous_prime_tick(
            agents,
            spine,
            tasks,
            reg,
            None,
            0,
            max,
            tenant,
            "echo",
            None,
            false,
            false,
            false,
            false,
            false,
            PrimePlanPackageTrigger::Tail,
        )
        .unwrap()
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
        autonomous_prime_tick(
            agents,
            spine,
            tasks,
            reg,
            None,
            0,
            max,
            tenant,
            hire_rig,
            None,
            false,
            false,
            false,
            false,
            false,
            PrimePlanPackageTrigger::Tail,
        )
        .unwrap()
    }

    // ── PRIME DELIBERATION v1 (scripted decider) ───────────────────────────
    // A test-only `PrimeAiDecider` that returns a fixed, scripted model reply so
    // the deliberation path is exercised end-to-end without a mesh or provider.

    /// Always returns the same raw model reply text (or a scripted unavailable
    /// error). Used to drive the deliberation layer deterministically in tests.
    struct ScriptedDecider {
        reply: Result<String, String>,
    }
    impl crate::nodes::coordinator::agent::prime_deliberation::PrimeAiDecider for ScriptedDecider {
        fn deliberate(&self, _prompt: &str) -> Result<String, String> {
            self.reply.clone()
        }
    }

    /// Run an autonomous Prime tick with deliberation ON and a scripted decider.
    fn tick_ai(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        reg: &crate::rig::RigRegistry,
        max: usize,
        tenant: Option<&str>,
        decider: &dyn crate::nodes::coordinator::agent::prime_deliberation::PrimeAiDecider,
    ) -> Vec<PrimeAutonomyRecord> {
        autonomous_prime_tick(
            agents,
            spine,
            tasks,
            reg,
            None,
            0,
            max,
            tenant,
            "echo",
            Some(decider),
            true,
            false,
            false,
            false,
            false,
            PrimePlanPackageTrigger::Tail,
        )
        .unwrap()
    }

    // DLB-1) A scripted `none` HOLDS the candidate: an otherwise auto-advanceable
    //        Mandate is skipped with ZERO side effects and the record reads
    //        llm_used.
    #[test]
    fn deliberation_none_holds_with_zero_side_effects() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        runnable_operative(&agents, "engineer", "subj-e");

        let decider = ScriptedDecider {
            reply: Ok(r#"{"action":"none","reason":"hold for human review"}"#.to_string()),
        };
        let recs = tick_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider);
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.phase, "needs_team_plan");
        assert_eq!(rec.action, "none");
        assert_eq!(rec.outcome, "skipped");
        assert_eq!(rec.ai_mode.as_deref(), Some("llm_used"));
        assert!(rec.reason.contains("hold"));
        // ZERO side effects — no Team Plan was recorded.
        assert!(spine.latest_team_plan("default", &m).unwrap().is_none());
    }

    // DLB-2) A scripted confirm of the computed action EXECUTES the governed
    //        action and the record reads llm_used.
    #[test]
    fn deliberation_confirm_executes_governed_action_llm_used() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        runnable_operative(&agents, "engineer", "subj-e");

        let decider = ScriptedDecider {
            reply: Ok(r#"{"action":"create_team_plan","reason":"crew is ready"}"#.to_string()),
        };
        let recs = tick_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider);
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.action, "create_team_plan");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.ai_mode.as_deref(), Some("llm_used"));
        // The governed action really ran.
        assert!(spine.latest_team_plan("default", &m).unwrap().is_some());
    }

    // DLB-3) Malformed / prose model output FALLS BACK deterministically: the
    //        legal deterministic action still executes and the record reads
    //        fallback.
    #[test]
    fn deliberation_malformed_output_falls_back_deterministically() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        runnable_operative(&agents, "engineer", "subj-e");

        let decider = ScriptedDecider {
            reply: Ok("Sure! I think you should plan the team now.".to_string()),
        };
        let recs = tick_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider);
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.action, "create_team_plan");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.ai_mode.as_deref(), Some("fallback"));
        // The deterministic action was NOT blocked by the bad model output.
        assert!(spine.latest_team_plan("default", &m).unwrap().is_some());
    }

    // DLB-4) A disallowed (known but out-of-set) action also falls back — the
    //        model cannot widen the legal choice.
    #[test]
    fn deliberation_disallowed_action_falls_back() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        runnable_operative(&agents, "engineer", "subj-e");

        // `start` is a real action but not legal for a needs_team_plan candidate.
        let decider = ScriptedDecider {
            reply: Ok(r#"{"action":"start","reason":"go"}"#.to_string()),
        };
        let recs = tick_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider);
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.action, "create_team_plan");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.ai_mode.as_deref(), Some("fallback"));
        assert!(spine.latest_team_plan("default", &m).unwrap().is_some());
    }

    // DLB-5) Model unavailable (decider returns an error) → deterministic action
    //        still executes and the record reads unavailable.
    #[test]
    fn deliberation_unavailable_runs_deterministically() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        runnable_operative(&agents, "engineer", "subj-e");

        let decider = ScriptedDecider {
            reply: Err("ai peer unreachable: timeout".to_string()),
        };
        let recs = tick_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider);
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.action, "create_team_plan");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.ai_mode.as_deref(), Some("unavailable"));
        assert!(spine.latest_team_plan("default", &m).unwrap().is_some());
    }

    // DLB-6) With deliberation ON but NO decider wired, the loop is honestly
    //        `unavailable` and the deterministic action still executes (the manual
    //        tick / missing-mesh shape).
    #[test]
    fn deliberation_enabled_without_decider_is_unavailable() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        runnable_operative(&agents, "engineer", "subj-e");

        let recs = autonomous_prime_tick(
            &agents,
            &spine,
            &tasks,
            &reg,
            None,
            0,
            1,
            Some("default"),
            "echo",
            None,
            true,
            false,
            false,
            false,
            false,
            PrimePlanPackageTrigger::Tail,
        )
        .unwrap();
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.ai_mode.as_deref(), Some("unavailable"));
        assert!(spine.latest_team_plan("default", &m).unwrap().is_some());
    }

    // DLB-7) Deliberation OFF leaves the record at deterministic_only (the loop is
    //        byte-for-byte the old behaviour).
    #[test]
    fn deliberation_off_is_deterministic_only() {
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
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.ai_mode.as_deref(), Some("deterministic_only"));
    }

    // DLB-8) The env flag parser honours the documented truthy set and defaults
    //        OFF.
    #[test]
    fn prime_llm_deliberation_flag_parsing() {
        for on in ["1", "true", "TRUE", "yes", "on", " On "] {
            assert!(
                parse_prime_llm_deliberation(Some(on)),
                "`{on}` should enable"
            );
        }
        for off in ["0", "false", "no", "off", "", "maybe"] {
            assert!(
                !parse_prime_llm_deliberation(Some(off)),
                "`{off}` should not"
            );
        }
        assert!(!parse_prime_llm_deliberation(None));
    }

    // ── PRIME STRATEGY AUTHORING v1 (scripted strategy drafter) ────────────
    // These exercise the *strategy body* authoring layer (separate from action
    // deliberation): deliberation is OFF, so the deterministic classifier still
    // chooses `propose_strategy`, and the scripted decider authors only the body.

    /// Run an autonomous Prime tick with deliberation OFF but strategy authoring
    /// ON and a scripted decider — so `propose_strategy`'s body is model-authored
    /// (or falls back) while the action choice stays deterministic.
    fn tick_strategy_ai(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        reg: &crate::rig::RigRegistry,
        max: usize,
        tenant: Option<&str>,
        decider: &dyn crate::nodes::coordinator::agent::prime_deliberation::PrimeAiDecider,
    ) -> Vec<PrimeAutonomyRecord> {
        autonomous_prime_tick(
            agents,
            spine,
            tasks,
            reg,
            None,
            0,
            max,
            tenant,
            "echo",
            Some(decider),
            false,
            true,
            false,
            false,
            false,
            PrimePlanPackageTrigger::Tail,
        )
        .unwrap()
    }

    /// A bare Mandate (status `planned`, no strategy) is discovered by the tick
    /// (`list_active_mandates` includes `planned`), so its next governed step is
    /// `propose_strategy`.
    const GOOD_STRATEGY_REPLY: &str = "# Strategy — Ship login\n\nThis is a DRAFT and is NOT \
approved.\n\n## Objective\nDeliver the login page wired to auth.\n\n## Risks\nApproval gates remain \
in force.\n";

    // STR-1) Strategy flag ON + a scripted GOOD draft → the proposed strategy
    //        carries the MODEL's content and the record reads strategy_ai_mode
    //        llm_used. The action choice stays deterministic (deliberation OFF).
    #[test]
    fn strategy_llm_authors_proposed_strategy_body() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = bare_mandate(&spine);

        let decider = ScriptedDecider {
            reply: Ok(GOOD_STRATEGY_REPLY.to_string()),
        };
        let recs = tick_strategy_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider);
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.phase, "needs_strategy_proposal");
        assert_eq!(rec.action, "propose_strategy");
        assert_eq!(rec.outcome, "advanced");
        // Action-choice provenance is deterministic; strategy-body provenance is llm.
        assert_eq!(rec.ai_mode.as_deref(), Some("deterministic_only"));
        assert_eq!(rec.strategy_ai_mode.as_deref(), Some("llm_used"));
        // The strategy is PROPOSED (never approved by the model).
        assert_eq!(
            spine.strategy_status("default", &m).unwrap().as_deref(),
            Some("proposed")
        );
        assert!(!spine.strategy_approved("default", &m).unwrap());
        // The proposed doc contains the model's authored content.
        let doc = spine.strategy_doc("default", &m).unwrap().unwrap();
        assert!(doc.contains("Deliver the login page wired to auth"));
    }

    // STR-2) Strategy flag ON + a scripted BAD (empty) draft → falls back to the
    //        deterministic body and the record reads strategy_ai_mode fallback.
    #[test]
    fn strategy_llm_bad_output_falls_back_deterministically() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = bare_mandate(&spine);

        let decider = ScriptedDecider {
            reply: Ok("   \n  ".to_string()),
        };
        let recs = tick_strategy_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider);
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.action, "propose_strategy");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.strategy_ai_mode.as_deref(), Some("fallback"));
        // Still proposed — the deterministic draft was used.
        assert_eq!(
            spine.strategy_status("default", &m).unwrap().as_deref(),
            Some("proposed")
        );
        let doc = spine.strategy_doc("default", &m).unwrap().unwrap();
        assert!(doc.contains("deterministic v1"));
    }

    // STR-2b) An overlong scripted draft is rejected by the validator → fallback.
    #[test]
    fn strategy_llm_overlong_output_falls_back() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = bare_mandate(&spine);

        let huge = "x".repeat(
            crate::nodes::coordinator::agent::prime_strategy::MAX_STRATEGY_OUTPUT_CHARS + 1,
        );
        let decider = ScriptedDecider { reply: Ok(huge) };
        let recs = tick_strategy_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider);
        let rec = recs.iter().find(|r| r.target_id == m).unwrap();
        assert_eq!(rec.strategy_ai_mode.as_deref(), Some("fallback"));
        let doc = spine.strategy_doc("default", &m).unwrap().unwrap();
        assert!(doc.contains("deterministic v1"));
    }

    // STR-3) Strategy flag ON but NO decider wired → record reads strategy_ai_mode
    //        unavailable and the deterministic draft is proposed.
    #[test]
    fn strategy_llm_no_decider_is_unavailable() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = bare_mandate(&spine);

        // Strategy ON but ai = None.
        let recs = autonomous_prime_tick(
            &agents,
            &spine,
            &tasks,
            &reg,
            None,
            0,
            1,
            Some("default"),
            "echo",
            None,
            false,
            true,
            false,
            false,
            false,
            PrimePlanPackageTrigger::Tail,
        )
        .unwrap();
        let rec = recs.iter().find(|r| r.target_id == m).unwrap();
        assert_eq!(rec.action, "propose_strategy");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.strategy_ai_mode.as_deref(), Some("unavailable"));
        assert_eq!(
            spine.strategy_status("default", &m).unwrap().as_deref(),
            Some("proposed")
        );
    }

    // STR-4) Strategy flag OFF → the deterministic draft is used and the
    //        propose_strategy row honestly reads strategy_ai_mode
    //        `deterministic_only` (the body author is the deterministic draft).
    #[test]
    fn strategy_llm_off_is_deterministic_only() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = bare_mandate(&spine);

        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs.iter().find(|r| r.target_id == m).unwrap();
        assert_eq!(rec.action, "propose_strategy");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.strategy_ai_mode.as_deref(), Some("deterministic_only"));
        let doc = spine.strategy_doc("default", &m).unwrap().unwrap();
        assert!(doc.contains("deterministic v1"));
    }

    // STR-5) A REJECTED strategy is never re-proposed/overwritten by the strategy
    //        authoring layer — the candidate is `blocked`, the strategy stays
    //        rejected, and the model is never consulted (no strategy provenance).
    #[test]
    fn strategy_llm_does_not_overwrite_a_rejected_strategy() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = bare_mandate(&spine);
        spine
            .propose_strategy("default", &m, "human draft")
            .unwrap();
        spine.reject_strategy("default", &m).unwrap();

        let decider = ScriptedDecider {
            reply: Ok(GOOD_STRATEGY_REPLY.to_string()),
        };
        let recs = tick_strategy_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider);
        let rec = recs.iter().find(|r| r.target_id == m).unwrap();
        // Not a propose_strategy step — the rejected strategy is a blocked human gate.
        assert_ne!(rec.action, "propose_strategy");
        assert!(rec.strategy_ai_mode.is_none());
        // Still rejected — never reset to proposed and never re-authored.
        assert_eq!(
            spine.strategy_status("default", &m).unwrap().as_deref(),
            Some("rejected")
        );
    }

    // STR-6) An ALREADY-PROPOSED strategy is not re-proposed: the next step is the
    //        (human) strategy-approval gate, not propose_strategy, so the strategy
    //        authoring layer never runs and never overwrites the existing doc.
    #[test]
    fn strategy_llm_does_not_overwrite_an_existing_proposed_strategy() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = bare_mandate(&spine);
        spine
            .propose_strategy("default", &m, "the human's own strategy")
            .unwrap();

        let decider = ScriptedDecider {
            reply: Ok(GOOD_STRATEGY_REPLY.to_string()),
        };
        let recs = tick_strategy_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider);
        let rec = recs.iter().find(|r| r.target_id == m).unwrap();
        assert_ne!(rec.action, "propose_strategy");
        assert!(rec.strategy_ai_mode.is_none());
        // The original proposed doc is untouched.
        let doc = spine.strategy_doc("default", &m).unwrap().unwrap();
        assert_eq!(doc, "the human's own strategy");
    }

    // STR-7) The strategy-draft env flag parser honours the truthy set, defaults OFF.
    #[test]
    fn prime_llm_strategy_draft_flag_parsing() {
        for on in ["1", "true", "TRUE", "yes", "on", " On "] {
            assert!(
                parse_prime_llm_strategy_draft(Some(on)),
                "`{on}` should enable"
            );
        }
        for off in ["0", "false", "no", "off", "", "maybe"] {
            assert!(
                !parse_prime_llm_strategy_draft(Some(off)),
                "`{off}` should not"
            );
        }
        assert!(!parse_prime_llm_strategy_draft(None));
    }

    // ── PRIME ORCHESTRATION AUTHORING v1 (scripted orchestration decider) ───
    // These exercise the ORCHESTRATION-TEXT layer (titles / dossiers / checklists)
    // — separate from action deliberation, strategy authoring, and prioritization
    // (all OFF here). The action choice + the whole skeleton (roles, agents,
    // assignments, ids, markers) stay deterministic; only the work-object TEXT of
    // newly-created Briefs is model-authored (or falls back).

    /// A ready-to-orchestrate Mandate: approved strategy + a recorded Team Plan
    /// whose single engineer hire is an already-active Operative — so the next
    /// governed step is `orchestrate_assign_ready`. Returns `(mandate_id,
    /// engineer_agent_id)` so a blueprint can key the subject by agent id.
    fn ready_to_orchestrate(
        spine: &SpineStore,
        agents: &AgentStore,
        tenant: &str,
    ) -> (String, String) {
        let m = spine
            .create_mandate(tenant, "Ship v1", "real product", None, None)
            .unwrap();
        spine.propose_strategy(tenant, &m, "build a team").unwrap();
        spine.approve_strategy(tenant, &m).unwrap();
        // Subject id is unique per tenant (agent_profiles.subject_id is globally
        // unique), so cross-tenant setups don't collide.
        let subject = format!("subj-w-{tenant}");
        let agent_id = agents
            .create_agent(
                "W", "engineer", "W", "eng", "eng", "prime", &subject, "medium", tenant,
            )
            .unwrap();
        let hires = format!("[{{\"role\":\"engineer\",\"agent_id\":\"{agent_id}\"}}]");
        spine
            .record_team_plan(&TeamPlanRecord {
                tenant_id: tenant,
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

    /// Run an autonomous Prime tick with ORCHESTRATION AUTHORING ON (deliberation +
    /// strategy + prioritization OFF) and a scripted decider — so the orchestration
    /// Brief TEXT is model-authored (or, with invalid output, deterministic) while
    /// the action choice + skeleton stay deterministic.
    fn tick_orch_ai(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        reg: &crate::rig::RigRegistry,
        max: usize,
        tenant: Option<&str>,
        decider: &dyn crate::nodes::coordinator::agent::prime_deliberation::PrimeAiDecider,
    ) -> Vec<PrimeAutonomyRecord> {
        autonomous_prime_tick(
            agents,
            spine,
            tasks,
            reg,
            None,
            0,
            max,
            tenant,
            "echo",
            Some(decider),
            false,
            false,
            false,
            true,
            false,
            PrimePlanPackageTrigger::Tail,
        )
        .unwrap()
    }

    // ORC-1) Orchestration flag ON + a scripted GOOD blueprint → the SAME skeleton
    //        is created/assigned (parent + role track + subject) but the
    //        newly-created Briefs carry the MODEL's title/dossier and the record
    //        reads orchestration_ai_mode llm_used. Action choice stays deterministic.
    #[test]
    fn orchestration_llm_authors_brief_text() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, agent_id) = ready_to_orchestrate(&spine, &agents, "default");

        let reply = format!(
            r#"{{"parent":{{"title":"Parent (LLM)","dossier":"Top plan."}},
                "roles":{{"engineer":{{"title":"Eng track (LLM)","dossier":"Build it.","checklist":["wire api","tests"]}}}},
                "subjects":{{"{agent_id}":{{"title":"Eng exec (LLM)","dossier":"Do the work."}}}}}}"#
        );
        let decider = ScriptedDecider { reply: Ok(reply) };
        let recs = tick_orch_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider);
        let rec = recs.iter().find(|r| r.target_id == m).expect("considered");
        assert_eq!(rec.phase, "needs_orchestration");
        assert_eq!(rec.action, "orchestrate_assign_ready");
        assert_eq!(rec.outcome, "advanced");
        // Action-choice provenance stays deterministic; orchestration text is llm.
        assert_eq!(rec.ai_mode.as_deref(), Some("deterministic_only"));
        assert_eq!(rec.orchestration_ai_mode.as_deref(), Some("llm_used"));

        // The SAME deterministic skeleton: parent + role track + subject = 3 Briefs.
        let cards = tasks.list_briefs_by_mandate(&m, 50).unwrap();
        assert_eq!(cards.len(), 3, "parent + role track + subject execution");

        // The newly-created Briefs carry the MODEL's titles (keyed by marker).
        let parent = tasks
            .get_brief_by_source_marker(&format!("mandate:{m}:parent"))
            .unwrap()
            .unwrap();
        assert_eq!(parent.title, "Parent (LLM)");
        let role = tasks
            .get_brief_by_source_marker(&format!("mandate:{m}:role:engineer"))
            .unwrap()
            .unwrap();
        assert_eq!(role.title, "Eng track (LLM)");
        let subject = tasks
            .get_brief_by_source_marker(&format!("mandate:{m}:role:engineer:subject:{agent_id}"))
            .unwrap()
            .unwrap();
        assert_eq!(subject.title, "Eng exec (LLM)");
        // The subject Brief is the one assigned (the skeleton/assignment is unchanged).
        assert_eq!(
            subject.assignee_agent_id.as_deref(),
            Some(agent_id.as_str())
        );
    }

    // ORC-2) Orchestration flag ON + a scripted BAD (malformed) blueprint → the
    //        deterministic titles are used and the record reads
    //        orchestration_ai_mode fallback. The skeleton is still created.
    #[test]
    fn orchestration_llm_bad_output_falls_back_deterministically() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, _agent_id) = ready_to_orchestrate(&spine, &agents, "default");

        let decider = ScriptedDecider {
            reply: Ok("Sure! Here is the plan you asked for.".to_string()),
        };
        let recs = tick_orch_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider);
        let rec = recs.iter().find(|r| r.target_id == m).expect("considered");
        assert_eq!(rec.action, "orchestrate_assign_ready");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.orchestration_ai_mode.as_deref(), Some("fallback"));
        // The deterministic titles were used.
        let parent = tasks
            .get_brief_by_source_marker(&format!("mandate:{m}:parent"))
            .unwrap()
            .unwrap();
        assert_eq!(parent.title, "Execute Mandate: Ship v1");
        let role = tasks
            .get_brief_by_source_marker(&format!("mandate:{m}:role:engineer"))
            .unwrap()
            .unwrap();
        assert_eq!(role.title, "Engineering track: Ship v1");
        assert_eq!(tasks.list_briefs_by_mandate(&m, 50).unwrap().len(), 3);
    }

    // ORC-3) Orchestration flag ON but NO decider wired → record reads
    //        orchestration_ai_mode unavailable and the deterministic tree is built.
    #[test]
    fn orchestration_llm_no_decider_is_unavailable() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, _agent_id) = ready_to_orchestrate(&spine, &agents, "default");

        // Orchestration ON but ai = None.
        let recs = autonomous_prime_tick(
            &agents,
            &spine,
            &tasks,
            &reg,
            None,
            0,
            1,
            Some("default"),
            "echo",
            None,
            false,
            false,
            false,
            true,
            false,
            PrimePlanPackageTrigger::Tail,
        )
        .unwrap();
        let rec = recs.iter().find(|r| r.target_id == m).expect("considered");
        assert_eq!(rec.action, "orchestrate_assign_ready");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.orchestration_ai_mode.as_deref(), Some("unavailable"));
        // Deterministic title + a created tree.
        let parent = tasks
            .get_brief_by_source_marker(&format!("mandate:{m}:parent"))
            .unwrap()
            .unwrap();
        assert_eq!(parent.title, "Execute Mandate: Ship v1");
        assert_eq!(tasks.list_briefs_by_mandate(&m, 50).unwrap().len(), 3);
    }

    // ORC-4) Orchestration flag OFF → the deterministic titles are used and the
    //        orchestrate row honestly reads orchestration_ai_mode
    //        `deterministic_only` (the text author is the deterministic helper).
    #[test]
    fn orchestration_llm_off_is_deterministic_only() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, _agent_id) = ready_to_orchestrate(&spine, &agents, "default");

        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs.iter().find(|r| r.target_id == m).expect("considered");
        assert_eq!(rec.action, "orchestrate_assign_ready");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(
            rec.orchestration_ai_mode.as_deref(),
            Some("deterministic_only")
        );
        let parent = tasks
            .get_brief_by_source_marker(&format!("mandate:{m}:parent"))
            .unwrap()
            .unwrap();
        assert_eq!(parent.title, "Execute Mandate: Ship v1");
    }

    // ORC-5) Rerun idempotency: a second orchestration tick with a DIFFERENT
    //        blueprint creates NO duplicate Briefs and never clobbers a
    //        hand-edited title (reuse is by source marker; titles are set on
    //        creation only).
    #[test]
    fn orchestration_llm_rerun_is_idempotent_and_preserves_hand_edits() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, agent_id) = ready_to_orchestrate(&spine, &agents, "default");

        let reply1 = format!(
            r#"{{"roles":{{"engineer":{{"title":"Eng track (LLM)"}}}},"subjects":{{"{agent_id}":{{"title":"Eng exec (LLM)"}}}}}}"#
        );
        let decider1 = ScriptedDecider { reply: Ok(reply1) };
        let _ = tick_orch_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider1);
        let after_first = tasks.list_briefs_by_mandate(&m, 50).unwrap().len();
        assert_eq!(after_first, 3);

        // A human renames the role-track Brief.
        let role_id = tasks
            .get_brief_by_source_marker(&format!("mandate:{m}:role:engineer"))
            .unwrap()
            .unwrap()
            .task_id;
        tasks
            .set_brief_field(&role_id, "title", "Human-edited track")
            .unwrap();

        // Re-run with a DIFFERENT blueprint title.
        let reply2 = format!(
            r#"{{"roles":{{"engineer":{{"title":"Eng track (LLM v2)"}}}},"subjects":{{"{agent_id}":{{"title":"Eng exec (LLM v2)"}}}}}}"#
        );
        let decider2 = ScriptedDecider { reply: Ok(reply2) };
        let _ = tick_orch_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider2);

        // No duplicate Briefs.
        assert_eq!(tasks.list_briefs_by_mandate(&m, 50).unwrap().len(), 3);
        // The hand-edited title is preserved (never clobbered by the rerun blueprint).
        let role = tasks
            .get_brief_by_source_marker(&format!("mandate:{m}:role:engineer"))
            .unwrap()
            .unwrap();
        assert_eq!(role.title, "Human-edited track");
    }

    // ORC-6) Cross-tenant safety: an orchestration tick scoped to one Guild only
    //        materialises that Guild's tree; another Guild's ready Mandate is
    //        untouched.
    #[test]
    fn orchestration_llm_is_tenant_scoped() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m_default, agent_default) = ready_to_orchestrate(&spine, &agents, "default");
        let (m_other, _agent_other) = ready_to_orchestrate(&spine, &agents, "other");

        let reply = format!(
            r#"{{"roles":{{"engineer":{{"title":"Eng (LLM)"}}}},"subjects":{{"{agent_default}":{{"title":"Exec (LLM)"}}}}}}"#
        );
        let decider = ScriptedDecider { reply: Ok(reply) };
        let recs = tick_orch_ai(&agents, &spine, &tasks, &reg, 5, Some("default"), &decider);

        // The default Guild's tree was built…
        assert!(recs.iter().any(|r| r.target_id == m_default));
        assert_eq!(
            tasks.list_briefs_by_mandate(&m_default, 50).unwrap().len(),
            3
        );
        // …the other Guild's Mandate was never considered or materialised.
        assert!(recs.iter().all(|r| r.target_id != m_other));
        assert!(
            tasks
                .list_briefs_by_mandate(&m_other, 50)
                .unwrap()
                .is_empty()
        );
    }

    // ORC-7) The orchestration-authoring env flag parser honours the truthy set,
    //        defaults OFF.
    #[test]
    fn prime_llm_orchestration_flag_parsing() {
        for on in ["1", "true", "TRUE", "yes", "on", " On "] {
            assert!(
                parse_prime_llm_orchestration(Some(on)),
                "`{on}` should enable"
            );
        }
        for off in ["0", "false", "no", "off", "", "maybe"] {
            assert!(
                !parse_prime_llm_orchestration(Some(off)),
                "`{off}` should not"
            );
        }
        assert!(!parse_prime_llm_orchestration(None));
    }

    // ── PRIME EXECUTIVE PRIORITIZATION v1 (scripted prioritization decider) ──
    // These exercise the QUEUE-ORDER layer (separate from action deliberation and
    // strategy authoring): both of those are OFF, so each candidate's action is
    // chosen deterministically; only the ORDER the tick spends its action budget
    // in is model-picked (or falls back).

    /// Run an autonomous Prime tick with PRIORITIZATION ON (deliberation +
    /// strategy authoring OFF) and an optional scripted decider — so the candidate
    /// ORDER is model-picked (or, with `None`/invalid output, deterministic).
    fn tick_prio(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        reg: &crate::rig::RigRegistry,
        max: usize,
        tenant: Option<&str>,
        decider: Option<&dyn crate::nodes::coordinator::agent::prime_deliberation::PrimeAiDecider>,
    ) -> Vec<PrimeAutonomyRecord> {
        autonomous_prime_tick(
            agents,
            spine,
            tasks,
            reg,
            None,
            0,
            max,
            tenant,
            "echo",
            decider,
            false,
            false,
            true,
            false,
            false,
            PrimePlanPackageTrigger::Tail,
        )
        .unwrap()
    }

    /// Two approved Mandates A (created first) and B (created second), each at
    /// `needs_team_plan` with `create_team_plan` attemptable. Discovery order is
    /// creation order, so the menu keys are `cand-1`=A, `cand-2`=B.
    fn two_actionable_mandates(agents: &AgentStore, spine: &SpineStore) -> (String, String) {
        runnable_operative(agents, "engineer", "subj-e");
        let a = approved_mandate(spine);
        let b = approved_mandate(spine);
        (a, b)
    }

    // PRI-1) Parser-level coverage lives in `prime_priority::tests`. PRI-2..7 below
    //        are the end-to-end loop behaviours.

    // PRI-2) Two actionable Mandates, max=1, the model picks the SECOND candidate →
    //        only the second advances; the first is left unchanged (skipped on the
    //        action budget) and the records read llm_used with a clear rank/order.
    #[test]
    fn prioritization_model_picks_second_only_second_advances() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (a, b) = two_actionable_mandates(&agents, &spine);

        // cand-2 == B (created second). The model elevates it above the
        // deterministic first.
        let decider = ScriptedDecider {
            reply: Ok(r#"{"order":["cand-2"],"reason":"ship B first"}"#.to_string()),
        };
        let recs = tick_prio(
            &agents,
            &spine,
            &tasks,
            &reg,
            1,
            Some("default"),
            Some(&decider),
        );

        let rec_b = recs
            .iter()
            .find(|r| r.target_id == b)
            .expect("B considered");
        assert_eq!(rec_b.action, "create_team_plan");
        assert_eq!(rec_b.outcome, "advanced");
        assert_eq!(rec_b.priority_ai_mode.as_deref(), Some("llm_used"));
        assert_eq!(rec_b.priority_rank, Some(1));
        // ONLY B was advanced — its Team Plan exists.
        assert!(spine.latest_team_plan("default", &b).unwrap().is_some());

        let rec_a = recs
            .iter()
            .find(|r| r.target_id == a)
            .expect("A considered");
        assert_eq!(rec_a.outcome, "skipped");
        assert!(rec_a.reason.contains("budget"), "A skipped on the budget");
        assert_eq!(rec_a.priority_ai_mode.as_deref(), Some("llm_used"));
        assert_eq!(rec_a.priority_rank, Some(2));
        // A is UNCHANGED — no Team Plan was recorded.
        assert!(spine.latest_team_plan("default", &a).unwrap().is_none());
    }

    // PRI-3) Invalid model output (prose) FALLS BACK to the deterministic order:
    //        the FIRST candidate advances and every record reads fallback.
    #[test]
    fn prioritization_invalid_output_falls_back_to_first() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (a, b) = two_actionable_mandates(&agents, &spine);

        let decider = ScriptedDecider {
            reply: Ok("Sure — I'd do B then A.".to_string()),
        };
        let recs = tick_prio(
            &agents,
            &spine,
            &tasks,
            &reg,
            1,
            Some("default"),
            Some(&decider),
        );

        // Deterministic first candidate (A) advanced; B is untouched.
        let rec_a = recs
            .iter()
            .find(|r| r.target_id == a)
            .expect("A considered");
        assert_eq!(rec_a.outcome, "advanced");
        assert_eq!(rec_a.priority_ai_mode.as_deref(), Some("fallback"));
        assert!(spine.latest_team_plan("default", &a).unwrap().is_some());
        assert!(spine.latest_team_plan("default", &b).unwrap().is_none());
    }

    // PRI-4) Unavailable model (decider errors) FALLS BACK to the deterministic
    //        order and records unavailable.
    #[test]
    fn prioritization_unavailable_model_falls_back_deterministic() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (a, b) = two_actionable_mandates(&agents, &spine);

        let decider = ScriptedDecider {
            reply: Err("ai peer unreachable: timeout".to_string()),
        };
        let recs = tick_prio(
            &agents,
            &spine,
            &tasks,
            &reg,
            1,
            Some("default"),
            Some(&decider),
        );

        let rec_a = recs
            .iter()
            .find(|r| r.target_id == a)
            .expect("A considered");
        assert_eq!(rec_a.outcome, "advanced");
        assert_eq!(rec_a.priority_ai_mode.as_deref(), Some("unavailable"));
        assert!(spine.latest_team_plan("default", &a).unwrap().is_some());
        assert!(spine.latest_team_plan("default", &b).unwrap().is_none());

        // With prioritization ON but NO decider wired the loop is also honestly
        // `unavailable` and runs deterministically.
        let (agents2, spine2, tasks2) = stores();
        let tasks2 = Arc::new(tasks2);
        let (a2, _b2) = two_actionable_mandates(&agents2, &spine2);
        let recs2 = tick_prio(&agents2, &spine2, &tasks2, &reg, 1, Some("default"), None);
        let rec_a2 = recs2.iter().find(|r| r.target_id == a2).unwrap();
        assert_eq!(rec_a2.outcome, "advanced");
        assert_eq!(rec_a2.priority_ai_mode.as_deref(), Some("unavailable"));
    }

    // PRI-5) An EMPTY order HOLDS the whole queue this tick: ZERO side effects,
    //        every offered candidate recorded skipped with llm_used.
    #[test]
    fn prioritization_empty_order_holds_zero_side_effects() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (a, b) = two_actionable_mandates(&agents, &spine);

        let decider = ScriptedDecider {
            reply: Ok(r#"{"order":[],"reason":"hold for human review"}"#.to_string()),
        };
        let recs = tick_prio(
            &agents,
            &spine,
            &tasks,
            &reg,
            1,
            Some("default"),
            Some(&decider),
        );

        for id in [&a, &b] {
            let rec = recs
                .iter()
                .find(|r| &r.target_id == id)
                .expect("considered");
            assert_eq!(rec.action, "none");
            assert_eq!(rec.outcome, "skipped");
            assert_eq!(rec.priority_ai_mode.as_deref(), Some("llm_used"));
            assert!(rec.reason.contains("hold"));
            // ZERO side effects — neither Mandate was planned.
            assert!(spine.latest_team_plan("default", id).unwrap().is_none());
        }
    }

    // PRI-6) A tenant-scoped tick only offers / acts on its OWN Guild's candidates;
    //        a candidate in another Guild is never discovered, offered, or touched.
    #[test]
    fn prioritization_is_tenant_scoped() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (_a, b) = two_actionable_mandates(&agents, &spine); // tenant "default"
        // A bare Mandate in another Guild — discoverable only under "other".
        let other = spine
            .create_mandate("other", "Other goal", "do not touch", None, None)
            .unwrap();

        let decider = ScriptedDecider {
            reply: Ok(r#"{"order":["cand-2"]}"#.to_string()),
        };
        let recs = tick_prio(
            &agents,
            &spine,
            &tasks,
            &reg,
            1,
            Some("default"),
            Some(&decider),
        );

        // No record belongs to another Guild.
        assert!(
            recs.iter().all(|r| r.tenant == "default"),
            "tenant-scoped tick must only consider its own Guild"
        );
        // The model's in-Guild pick (B) advanced…
        assert!(spine.latest_team_plan("default", &b).unwrap().is_some());
        // …and the other Guild's Mandate is completely untouched (no strategy, no plan).
        assert!(spine.strategy_status("other", &other).unwrap().is_none());
        assert!(spine.latest_team_plan("other", &other).unwrap().is_none());
    }

    // PRI-7) A PROPOSED Prime proposal is offered to the prioritization model ONLY
    //        when the Guild holds the `prime.proposal.approve` standing authority;
    //        without it the proposal is never offered (and stays proposed); with it
    //        the model may prioritize it.
    #[test]
    fn prioritization_offers_proposal_only_with_standing() {
        // Without standing → the proposal is never offered. The lone actionable
        // candidate (the approved Mandate) is driven deterministically (menu < 2),
        // and the proposal stays proposed.
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let pid = propose_pid(&agents, &spine);
        let m = approved_mandate(&spine);

        let decider = ScriptedDecider {
            reply: Ok(r#"{"order":["cand-1"]}"#.to_string()),
        };
        let recs = tick_prio(
            &agents,
            &spine,
            &tasks,
            &reg,
            1,
            Some("default"),
            Some(&decider),
        );
        // The proposal was not offered/approved — no proposal-approve record, and it
        // is still proposed.
        assert!(
            !recs
                .iter()
                .any(|r| r.target_id == pid && r.outcome == "approved"),
            "an unauthorized proposal is never approved"
        );
        assert_eq!(
            spine
                .get_prime_proposal("default", &pid)
                .unwrap()
                .unwrap()
                .status,
            "proposed"
        );
        // The approved Mandate is still driven (deterministically — only one
        // attemptable candidate, so the model is not consulted).
        let rec_m = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec_m.outcome, "advanced");

        // With standing → the proposal IS offered, and the model may prioritize it.
        let (agents2, spine2, tasks2) = stores();
        let tasks2 = Arc::new(tasks2);
        let pid2 = propose_pid(&agents2, &spine2);
        let _m2 = approved_mandate(&spine2);
        grant_standing(&agents2, "default", CATEGORY_PROPOSAL_APPROVE, Some(1));
        // cand-1 is the PASS-0 proposal (discovered first). The model elevates it.
        let decider2 = ScriptedDecider {
            reply: Ok(r#"{"order":["cand-1"],"reason":"materialize the plan first"}"#.to_string()),
        };
        let recs2 = tick_prio(
            &agents2,
            &spine2,
            &tasks2,
            &reg,
            1,
            Some("default"),
            Some(&decider2),
        );
        let rec_p = recs2
            .iter()
            .find(|r| r.target_id == pid2 && r.outcome == "approved")
            .expect("the prioritized proposal is approved");
        assert_eq!(rec_p.action, "approve");
        assert_eq!(rec_p.priority_ai_mode.as_deref(), Some("llm_used"));
        assert_eq!(rec_p.priority_rank, Some(1));
        assert_eq!(
            spine2
                .get_prime_proposal("default", &pid2)
                .unwrap()
                .unwrap()
                .status,
            "approved"
        );
    }

    // PRI-8) The prioritization env flag parser honours the truthy set, defaults OFF.
    #[test]
    fn prime_llm_prioritization_flag_parsing() {
        for on in ["1", "true", "TRUE", "yes", "on", " On "] {
            assert!(
                parse_prime_llm_prioritization(Some(on)),
                "`{on}` should enable"
            );
        }
        for off in ["0", "false", "no", "off", "", "maybe"] {
            assert!(
                !parse_prime_llm_prioritization(Some(off)),
                "`{off}` should not"
            );
        }
        assert!(!parse_prime_llm_prioritization(None));
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

    // B-strat) A bare Mandate with NO strategy is DRAFTED by the loop through the
    //    governed `propose_strategy` path: the strategy lands `proposed` (NOT
    //    approved), one action is consumed, and a second tick is idempotent (the
    //    proposed strategy is never overwritten — Prime then waits for a human).
    #[test]
    fn autonomous_tick_drafts_strategy_then_is_idempotent() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = bare_mandate(&spine);

        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.phase, "needs_strategy_proposal");
        assert_eq!(rec.action, "propose_strategy");
        assert_eq!(rec.outcome, "advanced");
        // Proposed, never approved — drafting is not approval.
        assert_eq!(
            spine.strategy_status("default", &m).unwrap().as_deref(),
            Some("proposed")
        );
        assert!(!spine.strategy_approved("default", &m).unwrap());

        // A second tick does NOT re-propose / overwrite — it now sees the human
        // approval gate and records `blocked` (no action), strategy unchanged.
        let recs2 = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec2 = recs2
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate re-considered");
        assert_eq!(rec2.phase, "needs_approval");
        assert_ne!(rec2.outcome, "advanced");
        assert_eq!(
            spine.strategy_status("default", &m).unwrap().as_deref(),
            Some("proposed")
        );
    }

    // B-strat2) A rejected strategy is never re-drafted by the loop (Prime does not
    //    fight a human rejection).
    #[test]
    fn autonomous_tick_does_not_redraft_rejected_strategy() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = bare_mandate(&spine);
        spine.propose_strategy("default", &m, "first").unwrap();
        spine.reject_strategy("default", &m).unwrap();

        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        if let Some(rec) = recs.iter().find(|r| r.target_id == m) {
            assert_ne!(rec.outcome, "advanced");
        }
        assert_eq!(
            spine.strategy_status("default", &m).unwrap().as_deref(),
            Some("rejected")
        );
    }

    // B-strat3) Tenant isolation — a tick scoped to another Guild never drafts this
    //    Guild's strategy.
    #[test]
    fn autonomous_tick_strategy_is_tenant_isolated() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = bare_mandate(&spine); // tenant "default"

        // A tick scoped to "other" touches nothing in "default".
        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("other"));
        assert!(recs.iter().all(|r| r.target_id != m));
        assert!(spine.strategy_status("default", &m).unwrap().is_none());
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

    // ── BARE-MANDATE AUTONOMOUS START (no owning Prime proposal) ───────────
    // A normal Mandate that reaches `ready_to_start` with NO owning proposal is
    // started by the autonomous loop ITSELF through the shared guarded run
    // pipeline (claims, adapter probe, durable ledger, budget hard-stop),
    // tenant-scoped and stamped as a heartbeat-trigger run.

    fn real_now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// A cost-bearing metric row attributing `cost_micros` of spend to `agent` in
    /// `tenant` at `ts_ms` — the additive Guild-budget gate sums these over the
    /// Guild's active roster.
    fn spend_row(
        agent: &str,
        tenant: &str,
        ts_ms: i64,
        cost_micros: u64,
    ) -> crate::metrics::InvocationMetric {
        crate::metrics::InvocationMetric {
            agent_name: agent.to_string(),
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

    /// Build a BARE Mandate (no owning Prime proposal) in `tenant` that is
    /// strategy-approved, team-planned, and at `ready_to_start`: an active,
    /// runnable engineer on the safe-local `echo` Rig, a Team Plan whose single
    /// required role re-resolves to that Operative (planned AND ready — no missing
    /// roles), and one assigned, unblocked, ready leaf Brief under the Mandate.
    /// Returns the Mandate id. (Built directly rather than through the orchestrate
    /// container tree, which leaves structural parent/track Briefs unassigned and
    /// so never classifies as `ready_to_start`.)
    fn ready_bare_mandate(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        tenant: &str,
    ) -> String {
        let eng = agents
            .ensure_starter_operative("engineer", "Eng", "Operative", "echo", tenant)
            .unwrap()
            .0;
        let m = spine
            .create_mandate(tenant, "Ship the login page", "wire it to auth", None, None)
            .unwrap();
        spine.propose_strategy(tenant, &m, "build a team").unwrap();
        spine.approve_strategy(tenant, &m).unwrap();
        spine
            .record_team_plan(&TeamPlanRecord {
                tenant_id: tenant,
                mandate_id: &m,
                actor_id: "operator",
                description: "build it",
                proposed_roles_json: "[\"engineer\"]",
                pending_hires_json: "[]",
                clearance_ids_json: "[]",
                denials_json: "[]",
                next_steps_json: "[]",
                status: "ready",
            })
            .unwrap();
        tasks
            .create_brief(
                tenant,
                "Wire login to auth",
                "operator",
                Some(&eng),
                Some(&m),
                None,
                None,
            )
            .unwrap();
        m
    }

    // H) A bare Mandate at ready_to_start is started by an autonomous tick — no
    //    owning proposal, no RELIX_HEARTBEAT_ENABLED, no manual brief.run — and the
    //    run is stamped as an autonomous heartbeat trigger, not dashboard manual.
    #[tokio::test]
    async fn autonomous_tick_starts_ready_bare_mandate() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = ready_bare_mandate(&agents, &spine, &tasks, "default");

        let recs = tick(&agents, &spine, &tasks, &reg, 5, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_kind == "mandate" && r.target_id == m && r.outcome == "started")
            .expect("a bare Mandate's ready work is started by the loop");
        assert_eq!(rec.phase, "ready_to_start");
        assert_eq!(rec.action, "start_mandate");

        // At least one durable run row, stamped as an autonomous heartbeat trigger.
        let runs = tasks.list_runs_for_tenant("default", 100).unwrap();
        assert!(!runs.is_empty(), "at least one durable run row was opened");
        assert!(
            runs.iter()
                .any(|r| r.trigger.as_deref() == Some("heartbeat")),
            "the autonomous start stamps a heartbeat-trigger run, not manual"
        );
        assert!(
            runs.iter().all(|r| r.trigger.as_deref() != Some("manual")),
            "no manual-trigger run is created by the autonomous bare-Mandate start"
        );
    }

    // I) Budget refusal blocks the WHOLE bare-Mandate autonomous start and opens
    //    ZERO run rows (the same conservative gate the proposal start applies).
    #[tokio::test]
    async fn autonomous_bare_mandate_start_blocked_over_budget_opens_no_runs() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = ready_bare_mandate(&agents, &spine, &tasks, "default");

        // Attribute the overspend to a ready Brief's assignee so the additive
        // Guild-budget gate (which sums the Guild's active roster) trips.
        let assignee = tasks
            .list_ready_briefs_for_tenant("default", 500)
            .unwrap()
            .into_iter()
            .find(|c| c.mandate_id.as_deref() == Some(m.as_str()))
            .and_then(|c| c.assignee_agent_id)
            .expect("a ready Brief with an assignee");

        // Guild budget $200; spend $250 → over budget.
        spine.set_guild_allowance("default", Some(20_000)).unwrap();
        let now = real_now_ms();
        let in_window = crate::nodes::coordinator::heartbeat::allowance_window(now).start_ms;
        let mstore = crate::metrics::MetricsStore::in_memory().unwrap();
        mstore
            .insert_batch(&[spend_row(&assignee, "default", in_window, 250_000_000)])
            .unwrap();
        let mq = crate::metrics::MetricsQuery::new(mstore);

        let recs = autonomous_prime_tick(
            &agents,
            &spine,
            &tasks,
            &reg,
            Some(&mq),
            now,
            5,
            Some("default"),
            "echo",
            None,
            false,
            false,
            false,
            false,
            false,
            PrimePlanPackageTrigger::Tail,
        )
        .unwrap();
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.phase, "ready_to_start");
        assert_eq!(rec.action, "start_mandate");
        assert_eq!(rec.outcome, "blocked");
        assert!(
            rec.reason.contains("budget"),
            "reason names the budget stop"
        );

        // The hard-stop opened ZERO runs.
        assert!(
            tasks
                .list_runs_for_tenant("default", 100)
                .unwrap()
                .is_empty(),
            "an over-budget bare-Mandate start opens no run rows"
        );
    }

    // J) Tenant isolation: a tick scoped to Guild "default" never selects or starts
    //    a ready bare Mandate that belongs to Guild "other".
    #[tokio::test]
    async fn autonomous_tick_does_not_start_cross_tenant_ready_bare_mandate() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m_other = ready_bare_mandate(&agents, &spine, &tasks, "other");

        let recs = tick(&agents, &spine, &tasks, &reg, 5, Some("default"));
        assert!(
            recs.iter().all(|r| r.target_id != m_other),
            "a `default` tick must not consider an `other` Mandate"
        );
        assert!(
            tasks.list_runs_for_tenant("other", 100).unwrap().is_empty(),
            "no cross-tenant Brief is started by a tenant-scoped tick"
        );
    }

    // K) A live claim / already-running Brief is not double-started: a second
    //    immediate tick leaves each already-started Brief with exactly one run row
    //    and records an honest non-started outcome.
    #[tokio::test]
    async fn autonomous_bare_mandate_does_not_double_start_running_briefs() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = ready_bare_mandate(&agents, &spine, &tasks, "default");

        let recs1 = tick(&agents, &spine, &tasks, &reg, 5, Some("default"));
        assert!(
            recs1
                .iter()
                .any(|r| r.target_id == m && r.outcome == "started"),
            "first tick starts the ready bare-Mandate work"
        );
        let count_by_brief = |t: &Arc<TaskStore>| {
            let mut map = std::collections::HashMap::new();
            for r in t.list_runs_for_tenant("default", 500).unwrap() {
                *map.entry(r.brief_id).or_insert(0usize) += 1;
            }
            map
        };
        let before = count_by_brief(&tasks);
        assert!(!before.is_empty(), "the first tick opened run rows");

        // A second immediate tick must not double-start an already-claimed/running
        // Brief — each previously-started Brief keeps exactly one run row.
        let recs2 = tick(&agents, &spine, &tasks, &reg, 5, Some("default"));
        let after = count_by_brief(&tasks);
        for (bid, n) in &before {
            assert_eq!(
                after.get(bid),
                Some(n),
                "Brief {bid} must not be double-started"
            );
        }
        if let Some(rec2) = recs2.iter().find(|r| r.target_id == m) {
            assert_ne!(
                rec2.outcome, "started",
                "a running bare Mandate is not re-started"
            );
        }
    }

    // ── MANUAL AUTONOMY TICK (operator `prime.autonomy_tick_now`) ──────────
    // An explicit operator wake-up that runs EXACTLY ONE bounded autonomous
    // Prime tick scoped to the caller's OWN Guild, through the same governed
    // `autonomous_prime_tick` path the timer uses. It does NOT require the
    // runtime switch to be on and grants no new authority.

    /// Run `handle_prime_autonomy_tick_now` for `tenant` as `role` (no metrics →
    /// the autonomous budget gate is inert). Returns the raw outcome so the
    /// deny-path test can assert POLICY_DENIED.
    fn tick_now_raw(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        reg: &crate::rig::RigRegistry,
        role: &str,
        tenant: &str,
    ) -> HandlerOutcome {
        let mut ctx = fake_ctx_with_role(b"", role, b"caller");
        ctx.tenant_id = Some(tenant.to_string());
        handle_prime_autonomy_tick_now(agents, spine, tasks, reg, None, &ctx)
    }

    /// Like [`tick_now_raw`] but as an operator, unwrapping the Ok JSON.
    fn tick_now(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        reg: &crate::rig::RigRegistry,
        tenant: &str,
    ) -> Value {
        match tick_now_raw(agents, spine, tasks, reg, "operator", tenant) {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("tick_now errored: {}", e.cause),
        }
    }

    // T-a) A worker / non-operator subject is DENIED tick-now (POLICY_DENIED) and
    //      no governed step runs as a side effect.
    #[test]
    fn tick_now_is_operator_only_no_side_effect() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        runnable_operative(&agents, "engineer", "subj-e");

        let out = tick_now_raw(&agents, &spine, &tasks, &reg, "worker", "default");
        match out {
            HandlerOutcome::Err(e) => {
                assert_eq!(e.kind, relix_core::types::error_kinds::POLICY_DENIED);
            }
            HandlerOutcome::Ok(_) => panic!("a worker subject must be denied tick-now"),
        }
        // The mandate was at needs_team_plan but no Team Plan was recorded — the
        // denied call mutated nothing.
        assert!(spine.latest_team_plan("default", &m).unwrap().is_none());
    }

    // T-b) Tenant scoping — a tick-now run by an operator in Guild "other" never
    //      touches Guild "default"'s bare Mandate or proposed proposal, even when
    //      "default" holds a proposal-approve standing grant (so it is the SCOPING,
    //      not a missing grant, that protects the other Guild).
    #[test]
    fn tick_now_is_tenant_scoped() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = bare_mandate(&spine); // "default", no strategy
        let pid = propose_pid(&agents, &spine); // "default", proposed
        grant_standing(&agents, "default", CATEGORY_PROPOSAL_APPROVE, Some(5));

        let v = tick_now(&agents, &spine, &tasks, &reg, "other");
        assert_eq!(v["tenant"], "other");
        let records = v["records"].as_array().unwrap();
        assert!(
            records
                .iter()
                .all(|r| r["target_id"] != json!(m) && r["target_id"] != json!(pid)),
            "an `other` tick must not consider `default` work"
        );
        // `default`'s bare Mandate got no strategy, and its proposal stays proposed.
        assert!(spine.strategy_status("default", &m).unwrap().is_none());
        assert_eq!(
            spine
                .get_prime_proposal("default", &pid)
                .unwrap()
                .unwrap()
                .status,
            "proposed"
        );
    }

    // T-c) Tick-now advances a same-tenant approved Mandate at needs_team_plan via
    //      the existing autonomous logic (the same governed team-plan route).
    #[test]
    fn tick_now_advances_same_tenant_approved_mandate() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        runnable_operative(&agents, "engineer", "subj-e");

        let v = tick_now(&agents, &spine, &tasks, &reg, "default");
        assert_eq!(v["tenant"], "default");
        let rec = v["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["target_id"] == json!(m))
            .expect("mandate considered");
        assert_eq!(rec["phase"], "needs_team_plan");
        assert_eq!(rec["action"], "create_team_plan");
        assert_eq!(rec["outcome"], "advanced");
        assert!(v["advanced"].as_u64().unwrap() >= 1);
        // A Team Plan now exists, recorded through the governed route.
        assert!(spine.latest_team_plan("default", &m).unwrap().is_some());
    }

    // T-d) Tick-now honors the SAME action budget as the timer and never exceeds
    //      RELIX_AUTONOMOUS_PRIME_MAX. With one MORE advanceable Mandate than the
    //      per-tick budget, exactly `max` advance and the excess is budget-skipped.
    //      (Computes `max` the same way the handler does, so it holds for any
    //      configured value without mutating the process env.)
    #[test]
    fn tick_now_honors_action_budget() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        runnable_operative(&agents, "engineer", "subj-e");
        let max = crate::nodes::coordinator::heartbeat::parse_autonomous_prime_max(
            std::env::var("RELIX_AUTONOMOUS_PRIME_MAX").ok().as_deref(),
        );
        // `max + 1` approved Mandates, each at needs_team_plan (advanceable).
        for _ in 0..=max {
            approved_mandate(&spine);
        }

        let v = tick_now(&agents, &spine, &tasks, &reg, "default");
        assert_eq!(v["max"].as_u64().unwrap() as usize, max);
        let advanced = v["advanced"].as_u64().unwrap() as usize;
        let started = v["started"].as_u64().unwrap() as usize;
        // Exactly `max` advanced even though `max + 1` Mandates were advanceable —
        // the per-tick budget capped it; the bound is never exceeded.
        assert_eq!(
            advanced, max,
            "advanced exactly the per-tick budget — never more"
        );
        assert_eq!(started, 0);
        assert!(
            advanced + started <= max,
            "a tick must never exceed RELIX_AUTONOMOUS_PRIME_MAX"
        );
        // The bound stopped the loop short of the excess candidate: only `max`
        // Team Plans were recorded across the `max + 1` Mandates.
        let team_planned = spine
            .list_active_mandates(Some("default"), 50)
            .unwrap()
            .iter()
            .filter(|m| {
                spine
                    .latest_team_plan("default", &m.mandate_id)
                    .unwrap()
                    .is_some()
            })
            .count();
        assert_eq!(
            team_planned, max,
            "the per-tick budget left the excess Mandate untouched"
        );
    }

    // ── MANUAL AUTONOMY TICK — LIVE DELIBERATION (operator) ────────────────
    // The manual tick (`prime.autonomy_tick_now`) closes the Prime Deliberation
    // v1 caveat: when the controller-runtime wires a live decider it exercises
    // the SAME deliberation layer as the timer. These tests drive
    // `handle_prime_autonomy_tick_now_with_ai` with a synchronous scripted
    // decider (no mesh / `Handle::block_on`), so the role gate + provenance are
    // covered without the async bridge.

    /// Run `handle_prime_autonomy_tick_now_with_ai` for `tenant` as `role` with an
    /// explicit optional decider + the deliberation switch. Returns the raw
    /// outcome so the deny-path test can assert POLICY_DENIED.
    #[allow(clippy::too_many_arguments)]
    fn tick_now_ai_raw(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        reg: &crate::rig::RigRegistry,
        role: &str,
        tenant: &str,
        ai: Option<&dyn crate::nodes::coordinator::agent::prime_deliberation::PrimeAiDecider>,
        llm_enabled: bool,
    ) -> HandlerOutcome {
        let mut ctx = fake_ctx_with_role(b"", role, b"caller");
        ctx.tenant_id = Some(tenant.to_string());
        handle_prime_autonomy_tick_now_with_ai(
            agents,
            spine,
            tasks,
            reg,
            None,
            &ctx,
            ai,
            llm_enabled,
            // Strategy authoring + prioritization + orchestration + plan-package
            // authoring off for the deliberation-focused manual-tick tests.
            false,
            false,
            false,
            false,
            PrimePlanPackageTrigger::Tail,
        )
    }

    /// Like [`tick_now_ai_raw`] but as an operator, unwrapping the Ok JSON.
    fn tick_now_ai(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        reg: &crate::rig::RigRegistry,
        tenant: &str,
        ai: Option<&dyn crate::nodes::coordinator::agent::prime_deliberation::PrimeAiDecider>,
        llm_enabled: bool,
    ) -> Value {
        match tick_now_ai_raw(
            agents,
            spine,
            tasks,
            reg,
            "operator",
            tenant,
            ai,
            llm_enabled,
        ) {
            HandlerOutcome::Ok(b) => serde_json::from_slice(&b).unwrap(),
            HandlerOutcome::Err(e) => panic!("tick_now_ai errored: {}", e.cause),
        }
    }

    // TA-1) Manual tick with a live decider scripted to HOLD (`{action:"none"}`)
    //       and llm_enabled=true: the record reads `ai_mode=llm_used`, the outcome
    //       is `skipped`, and there are ZERO side effects (no Team Plan recorded).
    #[test]
    fn tick_now_with_decider_hold_is_llm_used_zero_side_effects() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        runnable_operative(&agents, "engineer", "subj-e");

        let decider = ScriptedDecider {
            reply: Ok(r#"{"action":"none","reason":"hold for human review"}"#.to_string()),
        };
        let v = tick_now_ai(
            &agents,
            &spine,
            &tasks,
            &reg,
            "default",
            Some(&decider),
            true,
        );
        let rec = v["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["target_id"] == json!(m))
            .expect("mandate considered");
        assert_eq!(rec["phase"], "needs_team_plan");
        assert_eq!(rec["action"], "none");
        assert_eq!(rec["outcome"], "skipped");
        assert_eq!(rec["ai_mode"], "llm_used");
        // ZERO side effects — the HOLD recorded no Team Plan.
        assert!(spine.latest_team_plan("default", &m).unwrap().is_none());
    }

    // TA-2) Manual tick with a live decider scripted to CONFIRM the computed action
    //       executes the governed action and the record reads `ai_mode=llm_used`.
    #[test]
    fn tick_now_with_decider_confirm_executes_llm_used() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        runnable_operative(&agents, "engineer", "subj-e");

        let decider = ScriptedDecider {
            reply: Ok(r#"{"action":"create_team_plan","reason":"crew is ready"}"#.to_string()),
        };
        let v = tick_now_ai(
            &agents,
            &spine,
            &tasks,
            &reg,
            "default",
            Some(&decider),
            true,
        );
        let rec = v["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["target_id"] == json!(m))
            .expect("mandate considered");
        assert_eq!(rec["action"], "create_team_plan");
        assert_eq!(rec["outcome"], "advanced");
        assert_eq!(rec["ai_mode"], "llm_used");
        assert!(v["advanced"].as_u64().unwrap() >= 1);
        // The governed action really ran.
        assert!(spine.latest_team_plan("default", &m).unwrap().is_some());
    }

    // TA-3) Manual tick with NO decider but llm_enabled=true honestly reports
    //       `ai_mode=unavailable` and still executes the deterministic action — the
    //       missing-mesh / unreachable-peer fallback the caveat now narrows to.
    #[test]
    fn tick_now_no_decider_llm_enabled_is_unavailable_runs_deterministically() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        runnable_operative(&agents, "engineer", "subj-e");

        let v = tick_now_ai(&agents, &spine, &tasks, &reg, "default", None, true);
        let rec = v["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["target_id"] == json!(m))
            .expect("mandate considered");
        assert_eq!(rec["action"], "create_team_plan");
        assert_eq!(rec["outcome"], "advanced");
        assert_eq!(rec["ai_mode"], "unavailable");
        // The deterministic action was NOT blocked by the absent decider.
        assert!(spine.latest_team_plan("default", &m).unwrap().is_some());
    }

    // TA-4) The role gate is unchanged on the live-decider helper: a worker subject
    //       is POLICY_DENIED with zero side effects even with a decider wired.
    #[test]
    fn tick_now_with_decider_worker_denied_no_side_effect() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = approved_mandate(&spine);
        runnable_operative(&agents, "engineer", "subj-e");

        let decider = ScriptedDecider {
            reply: Ok(r#"{"action":"create_team_plan","reason":"go"}"#.to_string()),
        };
        let out = tick_now_ai_raw(
            &agents,
            &spine,
            &tasks,
            &reg,
            "worker",
            "default",
            Some(&decider),
            true,
        );
        match out {
            HandlerOutcome::Err(e) => {
                assert_eq!(e.kind, relix_core::types::error_kinds::POLICY_DENIED);
            }
            HandlerOutcome::Ok(_) => panic!("a worker subject must be denied tick-now"),
        }
        // The denied call mutated nothing — no Team Plan recorded.
        assert!(spine.latest_team_plan("default", &m).unwrap().is_none());
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

    // ── PRIME STRATEGY APPROVAL standing authority (v1) ────────────────────

    /// A Mandate carrying a PROPOSED (not-yet-approved) strategy in `tenant` —
    /// the `needs_approval` / `mandate.strategy.approve` shape the strategy-approve
    /// standing authority acts on.
    fn mandate_with_proposed_strategy(spine: &SpineStore, tenant: &str) -> String {
        let m = spine
            .create_mandate(tenant, "Ship v1", "real product", None, None)
            .unwrap();
        spine
            .propose_strategy(tenant, &m, "a proposed strategy")
            .unwrap();
        m
    }

    // SA-a) Without `prime.strategy.approve` standing authority the loop DRAFTS a
    //       strategy but never approves it: the draft lands `proposed`, and a
    //       re-tick leaves it at the human approval gate (blocked, not approved).
    #[test]
    fn autonomous_tick_drafts_strategy_but_does_not_approve_without_grant() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = bare_mandate(&spine);

        // Tick 1: draft a strategy proposal (proposed, NOT approved).
        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.action, "propose_strategy");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(
            spine.strategy_status("default", &m).unwrap().as_deref(),
            Some("proposed")
        );
        assert!(!spine.strategy_approved("default", &m).unwrap());

        // Tick 2: the next step is the human strategy-approval gate; with NO
        // standing authority it is left blocked, never approved.
        let recs2 = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec2 = recs2
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate re-considered");
        assert_eq!(rec2.phase, "needs_approval");
        assert_eq!(rec2.action, "none");
        assert_eq!(rec2.outcome, "blocked");
        assert!(!spine.strategy_approved("default", &m).unwrap());
    }

    // SA-b) With `prime.strategy.approve` standing authority the loop approves an
    //       already-PROPOSED strategy through the existing
    //       `mandate.strategy.approve` handler, consumes one bounded call, and the
    //       next governed step is no longer the strategy-approval gate.
    #[test]
    fn autonomous_tick_with_standing_approves_proposed_strategy() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = mandate_with_proposed_strategy(&spine, "default");
        grant_standing(&agents, "default", CATEGORY_STRATEGY_APPROVE, Some(1));

        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.phase, "needs_approval");
        assert_eq!(rec.action, "approve_strategy");
        assert_eq!(rec.outcome, "advanced");
        // Approved through the governed handler/store (proposed → approved).
        assert!(spine.strategy_approved("default", &m).unwrap());
        assert_eq!(
            spine.strategy_status("default", &m).unwrap().as_deref(),
            Some("approved")
        );
        // The bounded (max_calls=1) grant is now consumed.
        assert!(
            !agents
                .has_active_standing(AUTONOMOUS_PRIME_AUTHORITY, CATEGORY_STRATEGY_APPROVE, 1)
                .unwrap(),
            "a bounded strategy-approve grant is consumed when the approval is taken"
        );
        // The next governed step has moved off the strategy-approval gate.
        let after = next_step(&agents, &spine, &tasks, json!({ "mandate_id": m }));
        assert_ne!(after["phase"], "needs_approval");
        assert_ne!(after["action_api"], "mandate.strategy.approve");
    }

    // SA-c) Tenant isolation: a strategy-approve grant in Guild A never approves
    //       Guild B's proposed strategy (a cross-Guild tick approves only the
    //       granted Guild's strategy).
    #[test]
    fn standing_strategy_approve_is_tenant_isolated() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m_default = mandate_with_proposed_strategy(&spine, "default");
        let m_other = mandate_with_proposed_strategy(&spine, "other");
        // Grant ONLY in "default".
        grant_standing(&agents, "default", CATEGORY_STRATEGY_APPROVE, None);

        // Drive ALL Guilds.
        let _ = tick(&agents, &spine, &tasks, &reg, 10, None);

        assert!(
            spine.strategy_approved("default", &m_default).unwrap(),
            "the granted Guild's strategy is approved"
        );
        assert_eq!(
            spine.strategy_status("other", &m_other).unwrap().as_deref(),
            Some("proposed"),
            "a grant in `default` must never approve `other`'s strategy"
        );
    }

    // SA-d) A REJECTED strategy is never auto-approved or re-proposed, even WITH
    //       the grant — the store only flips `proposed` → `approved`, so a human
    //       rejection stays final and the grant is not consumed.
    #[test]
    fn standing_strategy_approve_never_approves_rejected() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = bare_mandate(&spine);
        spine.propose_strategy("default", &m, "first").unwrap();
        spine.reject_strategy("default", &m).unwrap();
        // Even WITH the grant, a rejected strategy is final.
        grant_standing(&agents, "default", CATEGORY_STRATEGY_APPROVE, Some(1));

        let recs = tick(&agents, &spine, &tasks, &reg, 5, Some("default"));
        if let Some(rec) = recs.iter().find(|r| r.target_id == m) {
            assert_ne!(rec.outcome, "advanced");
            assert_ne!(rec.action, "approve_strategy");
        }
        assert_eq!(
            spine.strategy_status("default", &m).unwrap().as_deref(),
            Some("rejected"),
            "a rejected strategy is never re-proposed or approved"
        );
        // The grant is untouched — no approval fired.
        assert!(
            agents
                .has_active_standing(AUTONOMOUS_PRIME_AUTHORITY, CATEGORY_STRATEGY_APPROVE, 1)
                .unwrap(),
            "no approval ⇒ the strategy-approve grant is not consumed"
        );
    }

    // SA-e) The per-tick action budget caps strategy approvals: with two proposed
    //       strategies and max=1, exactly one is approved; the budget-blocked one
    //       stays proposed and consumes NO grant call.
    #[test]
    fn standing_strategy_approve_respects_action_budget() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m1 = mandate_with_proposed_strategy(&spine, "default");
        let m2 = mandate_with_proposed_strategy(&spine, "default");
        // The grant has plenty of calls — the limiter under test is the per-tick
        // action budget (max=1), not the grant.
        grant_standing(&agents, "default", CATEGORY_STRATEGY_APPROVE, Some(5));

        let _ = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));

        let approved = [&m1, &m2]
            .iter()
            .filter(|m| spine.strategy_approved("default", m).unwrap())
            .count();
        assert_eq!(
            approved, 1,
            "the per-tick action budget caps approvals at max=1"
        );
        let still_proposed = [&m1, &m2]
            .iter()
            .filter(|m| spine.strategy_status("default", m).unwrap().as_deref() == Some("proposed"))
            .count();
        assert_eq!(
            still_proposed, 1,
            "the budget-blocked strategy stays proposed (never approved over budget)"
        );

        // Exactly one grant call was consumed — the budget-blocked candidate
        // (the tick stops at the action bound before reaching it) consumed none.
        let used = agents
            .list_standing_for_tenant(AUTONOMOUS_PRIME_AUTHORITY, "default")
            .unwrap()[0]
            .calls_used;
        assert_eq!(
            used, 1,
            "only the approval that actually fired consumed a grant call"
        );
    }

    // SA-f) Documents the chosen tick granularity: a bare Mandate DRAFTS on one
    //       tick and APPROVES on the NEXT. process_candidate takes at most one
    //       governed action per candidate per tick, so drafting + approving never
    //       collapse into a single tick even with max=2 and the grant present
    //       (minimal-invasive: no per-candidate re-classification loop was added).
    #[test]
    fn autonomous_tick_drafts_then_approves_strategy_across_two_ticks() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = bare_mandate(&spine);
        grant_standing(&agents, "default", CATEGORY_STRATEGY_APPROVE, None);

        // Tick 1 (max=2): draft only — does NOT also approve the same tick.
        let recs1 = tick(&agents, &spine, &tasks, &reg, 2, Some("default"));
        let rec1 = recs1.iter().find(|r| r.target_id == m).unwrap();
        assert_eq!(rec1.action, "propose_strategy");
        assert_eq!(rec1.outcome, "advanced");
        assert_eq!(
            spine.strategy_status("default", &m).unwrap().as_deref(),
            Some("proposed")
        );
        assert!(!spine.strategy_approved("default", &m).unwrap());

        // Tick 2: now approves the proposed strategy via standing authority.
        let recs2 = tick(&agents, &spine, &tasks, &reg, 2, Some("default"));
        let rec2 = recs2.iter().find(|r| r.target_id == m).unwrap();
        assert_eq!(rec2.action, "approve_strategy");
        assert_eq!(rec2.outcome, "advanced");
        assert!(spine.strategy_approved("default", &m).unwrap());
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

    // ── PRIME SHIFT DISPOSITION v1 (review_accept / apply_run) ─────────────────
    // Autonomous Prime closing the §12.6 review→apply tail under the two SEPARATE
    // standing grants. Both default OFF, both grant-gated, never combined.

    /// A `tenant` Mandate (approved strategy, ready team) with ONE assigned Brief
    /// whose latest Shift completed `done` and is parked in `pending_review` (the
    /// Brief moved to `in_review`). A scoped workspace is stamped so the run can
    /// later become apply-eligible. Returns `(mandate_id, brief_id, run_id)`.
    fn mandate_with_pending_review_shift(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        tenant: &str,
    ) -> (String, String, String) {
        let eng = agents
            .ensure_starter_operative("engineer", "Eng", "Operative", "echo", tenant)
            .unwrap()
            .0;
        let m = spine
            .create_mandate(tenant, "Ship the login page", "wire it to auth", None, None)
            .unwrap();
        spine.propose_strategy(tenant, &m, "build a team").unwrap();
        spine.approve_strategy(tenant, &m).unwrap();
        spine
            .record_team_plan(&TeamPlanRecord {
                tenant_id: tenant,
                mandate_id: &m,
                actor_id: "operator",
                description: "build it",
                proposed_roles_json: "[\"engineer\"]",
                pending_hires_json: "[]",
                clearance_ids_json: "[]",
                denials_json: "[]",
                next_steps_json: "[]",
                status: "ready",
            })
            .unwrap();
        let brief = tasks
            .create_brief(
                tenant,
                "Wire login to auth",
                "operator",
                Some(&eng),
                Some(&m),
                None,
                None,
            )
            .unwrap();
        let run_id = format!("run-{brief}");
        tasks
            .record_run_start(
                &run_id,
                &brief,
                &eng,
                "echo",
                "heartbeat",
                &crate::nodes::coordinator::RunWorkspaceInfo {
                    path: Some("ws"),
                    context: Some("empty"),
                    files: Some(0),
                    bytes: Some(0),
                },
            )
            .unwrap();
        // A `done` run opens `pending_review`; the completed Shift parks the Brief
        // in review (through the legal board path todo → in_progress → in_review;
        // `in_review` requires a stamped reviewer).
        tasks.record_run_finish(&run_id, "done", "ok").unwrap();
        tasks.set_brief_field(&brief, "reviewer", &eng).unwrap();
        tasks.set_board_status(&brief, "in_progress").unwrap();
        tasks.set_board_status(&brief, "in_review").unwrap();
        (m, brief, run_id)
    }

    fn run_of(tasks: &Arc<TaskStore>, run_id: &str) -> crate::nodes::coordinator::RunRecord {
        tasks.get_run(run_id).unwrap().expect("run row")
    }

    // SD-1) A `done` + `pending_review` run on a same-tenant Mandate Brief + a
    //       `prime.run.review_accept` grant → one tick ACCEPTS the review through
    //       the existing review path, consumes the bounded grant, records
    //       `review_accept`/`advanced`, and DOES NOT apply (apply state untouched).
    #[test]
    fn disposition_review_accept_with_grant_accepts_run() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, _brief, run_id) =
            mandate_with_pending_review_shift(&agents, &spine, &tasks, "default");
        grant_standing(&agents, "default", CATEGORY_RUN_REVIEW_ACCEPT, Some(1));

        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.phase, PHASE_NEEDS_REVIEW);
        assert_eq!(rec.action, ACTION_REVIEW_ACCEPT);
        assert_eq!(rec.outcome, "advanced");

        // Accepted through the existing review path — but NOT applied.
        let run = run_of(&tasks, &run_id);
        assert_eq!(run.review.as_deref(), Some("accepted"));
        assert_ne!(run.apply_status.as_deref(), Some("applied"));
        // The bounded (max_calls=1) review grant is consumed; the apply grant
        // (never granted) is not invented.
        assert!(
            !agents
                .has_active_standing(AUTONOMOUS_PRIME_AUTHORITY, CATEGORY_RUN_REVIEW_ACCEPT, 1)
                .unwrap(),
            "a bounded review_accept grant is consumed on a real acceptance"
        );
    }

    // SD-2) An already-accepted, apply-eligible run + a `prime.run.apply` grant →
    //       one tick APPLIES through the existing apply machinery, advances the
    //       Brief to `done`, consumes the grant, and records `apply_run`/`advanced`.
    #[test]
    fn disposition_apply_with_grant_applies_and_advances_brief() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief, run_id) =
            mandate_with_pending_review_shift(&agents, &spine, &tasks, "default");
        // Pre-accept the run (the review tick already happened); now only apply
        // remains. A zero-artifact run applies cleanly (no-op) through the real path.
        tasks.set_run_review(&run_id, "accepted", "ok").unwrap();
        grant_standing(&agents, "default", CATEGORY_RUN_APPLY, Some(1));

        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.phase, PHASE_NEEDS_APPLY);
        assert_eq!(rec.action, ACTION_APPLY_RUN);
        assert_eq!(rec.outcome, "advanced");

        // Applied through the existing machinery → the Brief advanced to done.
        let run = run_of(&tasks, &run_id);
        assert_eq!(run.apply_status.as_deref(), Some("applied"));
        assert_eq!(
            tasks.board_status(&brief).unwrap().as_deref(),
            Some("done"),
            "a clean apply closes the review-to-done on the board"
        );
        assert!(
            !agents
                .has_active_standing(AUTONOMOUS_PRIME_AUTHORITY, CATEGORY_RUN_APPLY, 1)
                .unwrap(),
            "a bounded apply grant is consumed on a real apply"
        );
    }

    // SD-3) NO grant → the completed Shift is a human gate: the tick records
    //       blocked and the run is left exactly as it was (pending_review, no
    //       apply), and NO disposition action is taken.
    #[test]
    fn disposition_no_grant_leaves_completed_shift_untouched() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief, run_id) =
            mandate_with_pending_review_shift(&agents, &spine, &tasks, "default");

        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.phase, PHASE_NEEDS_REVIEW);
        assert_eq!(rec.action, "none");
        assert_eq!(rec.outcome, "blocked");

        let run = run_of(&tasks, &run_id);
        assert_eq!(run.review.as_deref(), Some("pending_review"));
        assert_ne!(run.apply_status.as_deref(), Some("applied"));
        assert_eq!(
            tasks.board_status(&brief).unwrap().as_deref(),
            Some("in_review"),
            "no grant ⇒ the Brief stays in review"
        );
    }

    // SD-4) Rejected / already-applied / non-`done` (failed, running) latest runs
    //       are NEVER selected for disposition even with BOTH grants live.
    #[test]
    fn disposition_skips_rejected_failed_running_and_applied_runs() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief, run_id) =
            mandate_with_pending_review_shift(&agents, &spine, &tasks, "default");
        grant_standing(&agents, "default", CATEGORY_RUN_REVIEW_ACCEPT, Some(5));
        grant_standing(&agents, "default", CATEGORY_RUN_APPLY, Some(5));

        // (a) rejected — a human decision, never auto-touched.
        tasks.set_run_review(&run_id, "rejected", "no").unwrap();
        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs.iter().find(|r| r.target_id == m).unwrap();
        assert_ne!(rec.action, ACTION_REVIEW_ACCEPT);
        assert_ne!(rec.action, ACTION_APPLY_RUN);
        assert_eq!(run_of(&tasks, &run_id).review.as_deref(), Some("rejected"));

        // (b) accepted + already applied — terminal apply, never re-applied.
        tasks.set_run_review(&run_id, "accepted", "ok").unwrap();
        tasks
            .set_run_apply_status(&run_id, "applied", "done", 0, 0)
            .unwrap();
        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs.iter().find(|r| r.target_id == m).unwrap();
        assert_ne!(rec.action, ACTION_APPLY_RUN);

        // (c) a non-`done` latest run (failed) is invisible to disposition. A
        //     fresh later run supersedes the prior one as the Brief's latest.
        let run2 = format!("{run_id}-2");
        tasks
            .record_run_start(
                &run2,
                &brief,
                "agent",
                "echo",
                "heartbeat",
                &crate::nodes::coordinator::RunWorkspaceInfo {
                    path: Some("ws"),
                    context: Some("empty"),
                    files: Some(0),
                    bytes: Some(0),
                },
            )
            .unwrap();
        tasks.record_run_finish(&run2, "failed", "boom").unwrap();
        assert!(
            disposition_candidate(&tasks, "default", std::slice::from_ref(&brief)).is_none(),
            "a failed latest run is never a disposition candidate"
        );
        // And a still-running latest run is likewise invisible.
        let run3 = format!("{run_id}-3");
        tasks
            .record_run_start(
                &run3,
                &brief,
                "agent",
                "echo",
                "heartbeat",
                &crate::nodes::coordinator::RunWorkspaceInfo {
                    path: Some("ws"),
                    context: Some("empty"),
                    files: Some(0),
                    bytes: Some(0),
                },
            )
            .unwrap();
        assert!(
            disposition_candidate(&tasks, "default", &[brief]).is_none(),
            "a running latest run is never a disposition candidate"
        );
    }

    // SD-5) Cross-tenant invisibility: a run is never selected under another
    //       Guild's tenant, and a default-scoped tick with a default grant never
    //       touches Guild "other"'s completed Shift.
    #[test]
    fn disposition_candidate_is_tenant_scoped() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (_m, brief, run_id) =
            mandate_with_pending_review_shift(&agents, &spine, &tasks, "other");

        // The helper guards each run with `run_belongs_to_tenant`: a "default"
        // view never sees Guild "other"'s run.
        assert!(
            disposition_candidate(&tasks, "default", std::slice::from_ref(&brief)).is_none(),
            "cross-tenant run is invisible to a different Guild's disposition"
        );
        assert_eq!(
            disposition_candidate(&tasks, "other", &[brief]),
            Some((PHASE_NEEDS_REVIEW, run_id.clone())),
            "the owning Guild does see it"
        );

        // A default-scoped tick (even holding a default review grant) never drives
        // the "other" Guild's Mandate or accepts its run.
        grant_standing(&agents, "default", CATEGORY_RUN_REVIEW_ACCEPT, Some(5));
        let recs = tick(&agents, &spine, &tasks, &reg, 5, Some("default"));
        assert!(
            recs.iter().all(|r| r.tenant != "other"),
            "a default-scoped tick never processes the other Guild"
        );
        assert_eq!(
            run_of(&tasks, &run_id).review.as_deref(),
            Some("pending_review"),
            "the other Guild's run is untouched"
        );
    }

    // SD-6) An apply that conflicts (unsafe/missing-source plan) records `blocked`,
    //       does NOT mark the Brief done, and does NOT consume the apply grant —
    //       and there is no blind in-tick retry.
    #[test]
    fn disposition_apply_conflict_blocks_and_does_not_mark_brief_done() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief, run_id) =
            mandate_with_pending_review_shift(&agents, &spine, &tasks, "default");
        tasks.set_run_review(&run_id, "accepted", "ok").unwrap();
        // A `created` artifact whose source file is missing in the (bogus) run
        // workspace makes the safe-apply plan non-applicable → conflicted. The plan
        // is read-only, so nothing is written to the project root.
        tasks
            .record_run_artifact(
                &run_id,
                &brief,
                "relix-nonexistent-disposition-ws",
                "relix_disposition_missing_source.txt",
                "created",
                1,
                Some("deadbeef"),
                None,
                true,
            )
            .unwrap();
        grant_standing(&agents, "default", CATEGORY_RUN_APPLY, Some(1));

        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.phase, PHASE_NEEDS_APPLY);
        assert_eq!(rec.action, ACTION_APPLY_RUN);
        assert_eq!(rec.outcome, "blocked");

        // The Brief is NOT done, and the bounded apply grant is NOT consumed.
        assert_ne!(
            tasks.board_status(&brief).unwrap().as_deref(),
            Some("done"),
            "a conflicted apply never closes review-to-done"
        );
        assert_ne!(
            run_of(&tasks, &run_id).apply_status.as_deref(),
            Some("applied")
        );
        assert!(
            agents
                .has_active_standing(AUTONOMOUS_PRIME_AUTHORITY, CATEGORY_RUN_APPLY, 1)
                .unwrap(),
            "a blocked apply does not consume the grant"
        );
    }

    // SD-7) LLM `none` / hold causes ZERO disposition side effects even with the
    //       grant live: the run stays pending_review and nothing is accepted.
    #[test]
    fn disposition_llm_hold_causes_zero_side_effects() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, _brief, run_id) =
            mandate_with_pending_review_shift(&agents, &spine, &tasks, "default");
        grant_standing(&agents, "default", CATEGORY_RUN_REVIEW_ACCEPT, Some(1));

        let decider = ScriptedDecider {
            reply: Ok(r#"{"action":"none","reason":"hold for human review"}"#.to_string()),
        };
        let recs = tick_ai(&agents, &spine, &tasks, &reg, 1, Some("default"), &decider);
        let rec = recs
            .iter()
            .find(|r| r.target_id == m)
            .expect("mandate considered");
        assert_eq!(rec.action, "none");
        assert_eq!(rec.outcome, "skipped");
        assert_eq!(rec.ai_mode.as_deref(), Some("llm_used"));

        // ZERO side effects — the run is not accepted and the grant is not consumed.
        assert_eq!(
            run_of(&tasks, &run_id).review.as_deref(),
            Some("pending_review")
        );
        assert!(
            agents
                .has_active_standing(AUTONOMOUS_PRIME_AUTHORITY, CATEGORY_RUN_REVIEW_ACCEPT, 1)
                .unwrap(),
            "a held tick consumes no grant"
        );
    }

    // SD-8) Review and apply are SEPARATE ticks: with both grants live, tick 1
    //       accepts the pending_review run (no apply yet); tick 2 applies the now-
    //       accepted run and advances the Brief. A single tick never does both.
    #[test]
    fn disposition_review_then_apply_over_two_ticks() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief, run_id) =
            mandate_with_pending_review_shift(&agents, &spine, &tasks, "default");
        grant_standing(&agents, "default", CATEGORY_RUN_REVIEW_ACCEPT, Some(5));
        grant_standing(&agents, "default", CATEGORY_RUN_APPLY, Some(5));

        // Tick 1 — accept only.
        let recs1 = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec1 = recs1.iter().find(|r| r.target_id == m).unwrap();
        assert_eq!(rec1.action, ACTION_REVIEW_ACCEPT);
        assert_eq!(rec1.outcome, "advanced");
        assert_eq!(run_of(&tasks, &run_id).review.as_deref(), Some("accepted"));
        assert_ne!(
            run_of(&tasks, &run_id).apply_status.as_deref(),
            Some("applied"),
            "tick 1 accepts but never applies the same run"
        );
        assert_ne!(tasks.board_status(&brief).unwrap().as_deref(), Some("done"));

        // Tick 2 — apply the now-accepted run.
        let recs2 = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        let rec2 = recs2.iter().find(|r| r.target_id == m).unwrap();
        assert_eq!(rec2.action, ACTION_APPLY_RUN);
        assert_eq!(rec2.outcome, "advanced");
        assert_eq!(
            run_of(&tasks, &run_id).apply_status.as_deref(),
            Some("applied")
        );
        assert_eq!(tasks.board_status(&brief).unwrap().as_deref(), Some("done"));
    }

    // ── PRIME PLAN-PACKAGE AUTHORING v1 (opt-in, default OFF) ───────────────

    /// A scripted-valid plan-package reply (2 children, one with a backward dep).
    fn pp_reply() -> String {
        serde_json::json!({
            "plan_title": "Login plan",
            "plan_body": "# Plan\n\nDecompose the login work into steps.",
            "summary": "Decompose the login page",
            "children": [
                {"title": "Wire the form", "priority": "high"},
                {"title": "Hook up auth", "after": 0}
            ]
        })
        .to_string()
    }

    /// Build a candidate Mandate in `tenant` that the existing governed flow leaves
    /// IDLE on the plan-package tail: strategy-approved + team-ready (so it is past
    /// every advance/gate), with exactly ONE un-decomposed Brief that is BLOCKED
    /// (so it is not `ready_to_start` and the tick reaches the (B5) plan-package
    /// step). Returns `(mandate_id, brief_id)`.
    fn idle_single_brief_mandate(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        tenant: &str,
        seed: &str,
    ) -> (String, String) {
        let eng = agents
            .request_hire(
                "W", "engineer", "W", "eng", "eng", "prime", seed, "medium", tenant,
            )
            .unwrap();
        agents
            .approve_hire_with_rig(&eng, Some("echo"), tenant)
            .unwrap();
        let m = spine
            .create_mandate(tenant, "Ship the login page", "wire it to auth", None, None)
            .unwrap();
        spine.propose_strategy(tenant, &m, "build a team").unwrap();
        spine.approve_strategy(tenant, &m).unwrap();
        spine
            .record_team_plan(&TeamPlanRecord {
                tenant_id: tenant,
                mandate_id: &m,
                actor_id: "operator",
                description: "build it",
                proposed_roles_json: "[\"engineer\"]",
                pending_hires_json: "[]",
                clearance_ids_json: "[]",
                denials_json: "[]",
                next_steps_json: "[]",
                status: "ready",
            })
            .unwrap();
        let brief = tasks
            .create_brief(
                tenant,
                "Ship the login page",
                "operator",
                Some(&eng),
                Some(&m),
                None,
                None,
            )
            .unwrap();
        // A separate, un-linked blocker Brief makes the candidate Brief `blocked`
        // (so the tick reaches the plan-package tail instead of `ready_to_start`).
        // It carries no mandate_id, so the candidate Mandate still has exactly one
        // Brief.
        let blocker = tasks
            .create_brief(tenant, "blocker", "operator", None, None, None, None)
            .unwrap();
        tasks.add_snag(&brief, &blocker).unwrap();
        (m, brief)
    }

    /// Run one autonomous tick with the plan-package switch ON (and an optional live
    /// decider), other LLM switches off, `echo` hire Rig, no budget gate. The v1
    /// `tail` trigger — author a plan package only on the IDLE tail (never preempt a
    /// start).
    fn tick_pp(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        reg: &crate::rig::RigRegistry,
        max: usize,
        tenant: Option<&str>,
        ai: Option<&dyn crate::nodes::coordinator::agent::prime_deliberation::PrimeAiDecider>,
    ) -> Vec<PrimeAutonomyRecord> {
        tick_pp_trig(
            agents,
            spine,
            tasks,
            reg,
            max,
            tenant,
            ai,
            PrimePlanPackageTrigger::Tail,
        )
    }

    /// Like [`tick_pp`] but with an explicit plan-package trigger — `Tail` (v1
    /// idle gap-fill) or `BeforeExecute` (v2 active planner, preempt a raw start).
    #[allow(clippy::too_many_arguments)]
    fn tick_pp_trig(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        reg: &crate::rig::RigRegistry,
        max: usize,
        tenant: Option<&str>,
        ai: Option<&dyn crate::nodes::coordinator::agent::prime_deliberation::PrimeAiDecider>,
        trigger: PrimePlanPackageTrigger,
    ) -> Vec<PrimeAutonomyRecord> {
        autonomous_prime_tick(
            agents, spine, tasks, reg, None, 0, max, tenant, "echo", ai, false, false, false,
            false, true, trigger,
        )
        .unwrap()
    }

    fn open_interactions(tasks: &TaskStore, brief: &str) -> (usize, usize) {
        let ix = tasks.list_interactions(brief).unwrap();
        let suggestions = ix
            .iter()
            .filter(|i| i.kind == "suggest_tasks" && i.status == "open")
            .count();
        let confirms = ix
            .iter()
            .filter(|i| {
                i.kind == "confirm"
                    && i.status == "open"
                    && i.bound_doc_kind.as_deref() == Some("plan")
            })
            .count();
        (suggestions, confirms)
    }

    // PP1) An eligible idle candidate gets EXACTLY ONE model-authored plan package,
    //      and the confirm is left OPEN (never self-approved).
    #[test]
    fn plan_package_opens_one_and_leaves_confirm_open() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief) = idle_single_brief_mandate(&agents, &spine, &tasks, "default", "pp1");

        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        let recs = tick_pp(
            &agents,
            &spine,
            &tasks,
            &reg,
            1,
            Some("default"),
            Some(&decider),
        );
        let rec = recs
            .iter()
            .find(|r| r.target_id == m && r.action == "plan_package")
            .expect("the idle candidate gets a plan-package action");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.plan_package_ai_mode.as_deref(), Some("llm_used"));
        assert_eq!(rec.child_count, Some(2));
        assert!(rec.plan_doc_id.is_some());
        assert!(rec.suggestion_id.is_some());
        assert!(rec.confirm_id.is_some());

        // The package exists: a plan Dossier + an OPEN suggest_tasks + an OPEN
        // plan-bound confirm. The confirm is NOT resolved — no self-approval.
        assert!(tasks.latest_dossier(&brief, "plan").unwrap().is_some());
        assert_eq!(open_interactions(&tasks, &brief), (1, 1));
        // No children materialized yet (approval is still pending).
        assert!(tasks.list_subbriefs(&brief).unwrap().is_empty());
    }

    // PP2) A second tick does NOT open a duplicate package — it reports the existing
    //      open package and authors nothing new.
    #[test]
    fn plan_package_no_duplicate_on_second_tick() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief) = idle_single_brief_mandate(&agents, &spine, &tasks, "default", "pp2");
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };

        let _ = tick_pp(
            &agents,
            &spine,
            &tasks,
            &reg,
            1,
            Some("default"),
            Some(&decider),
        );
        assert_eq!(open_interactions(&tasks, &brief), (1, 1));

        let recs2 = tick_pp(
            &agents,
            &spine,
            &tasks,
            &reg,
            1,
            Some("default"),
            Some(&decider),
        );
        let rec2 = recs2
            .iter()
            .find(|r| r.target_id == m && r.action == "plan_package")
            .expect("a plan-package record on the re-tick");
        assert_eq!(rec2.outcome, "skipped");
        // Dedup-guarded: a re-tick refuses to author a second package — either
        // because the plan Dossier now exists or because the open package awaits
        // approval (the eligibility check reports the first guard it hits).
        assert!(rec2.reason.contains("already"), "got: {}", rec2.reason);
        // Still exactly one open package — no duplicate.
        assert_eq!(open_interactions(&tasks, &brief), (1, 1));
    }

    // PP3) Malformed model output falls back to a deterministic SAFE package (no
    //      partial broken package); an absent decider is honestly `unavailable`.
    #[test]
    fn plan_package_malformed_or_unavailable_falls_back_safely() {
        // Malformed JSON → deterministic fallback content, one valid package.
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief) = idle_single_brief_mandate(&agents, &spine, &tasks, "default", "pp3a");
        let bad = ScriptedDecider {
            reply: Ok("not json at all".to_string()),
        };
        let recs = tick_pp(
            &agents,
            &spine,
            &tasks,
            &reg,
            1,
            Some("default"),
            Some(&bad),
        );
        let rec = recs
            .iter()
            .find(|r| r.target_id == m && r.action == "plan_package")
            .unwrap();
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.plan_package_ai_mode.as_deref(), Some("fallback"));
        assert_eq!(rec.child_count, Some(3)); // the deterministic 3-step chain
        assert_eq!(open_interactions(&tasks, &brief), (1, 1));

        // No decider wired → unavailable, still a deterministic package.
        let (agents2, spine2, tasks2) = stores();
        let tasks2 = Arc::new(tasks2);
        let (m2, brief2) = idle_single_brief_mandate(&agents2, &spine2, &tasks2, "default", "pp3b");
        let recs2 = tick_pp(&agents2, &spine2, &tasks2, &reg, 1, Some("default"), None);
        let rec2 = recs2
            .iter()
            .find(|r| r.target_id == m2 && r.action == "plan_package")
            .unwrap();
        assert_eq!(rec2.outcome, "advanced");
        assert_eq!(rec2.plan_package_ai_mode.as_deref(), Some("unavailable"));
        assert_eq!(open_interactions(&tasks2, &brief2), (1, 1));
    }

    // PP4) An existing `plan` Dossier (e.g. human-authored) is NEVER clobbered — the
    //      tick reports blocked and authors no package.
    #[test]
    fn plan_package_does_not_clobber_existing_plan() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief) = idle_single_brief_mandate(&agents, &spine, &tasks, "default", "pp4");
        // A human writes a plan Dossier first.
        tasks
            .add_dossier(&brief, "plan", "Human plan", "do it by hand")
            .unwrap();

        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        let recs = tick_pp(
            &agents,
            &spine,
            &tasks,
            &reg,
            1,
            Some("default"),
            Some(&decider),
        );
        let rec = recs
            .iter()
            .find(|r| r.target_id == m && r.action == "plan_package")
            .unwrap();
        assert_eq!(rec.outcome, "skipped");
        assert!(rec.reason.contains("already exists"), "got: {}", rec.reason);
        // No package opened, and the human plan is intact (unchanged title).
        assert_eq!(open_interactions(&tasks, &brief), (0, 0));
        assert_eq!(
            tasks.latest_dossier(&brief, "plan").unwrap().unwrap().title,
            "Human plan"
        );
    }

    // PP5) Tenant isolation — a tick scoped to another Guild never authors a package
    //      on this Guild's Brief.
    #[test]
    fn plan_package_tenant_isolation() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (_m, brief) = idle_single_brief_mandate(&agents, &spine, &tasks, "guild-a", "pp5");
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        // Drive a DIFFERENT Guild — guild-a's Brief must be untouched.
        let _ = tick_pp(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("guild-b"),
            Some(&decider),
        );
        assert_eq!(open_interactions(&tasks, &brief), (0, 0));
        assert!(tasks.latest_dossier(&brief, "plan").unwrap().is_none());
    }

    // PP6) An approved plan package materializes its children through the EXISTING
    //      exactly-once decomposition ledger (a second accept returns the SAME ids).
    #[test]
    fn plan_package_approval_materializes_exactly_once() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief) = idle_single_brief_mandate(&agents, &spine, &tasks, "default", "pp6");
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        let recs = tick_pp(
            &agents,
            &spine,
            &tasks,
            &reg,
            1,
            Some("default"),
            Some(&decider),
        );
        let rec = recs
            .iter()
            .find(|r| r.target_id == m && r.action == "plan_package")
            .unwrap();
        let confirm = rec.confirm_id.clone().unwrap();
        let n = rec.child_count.unwrap();

        // Approve through the existing plan-confirm path (children carry no assignee
        // hints, so resolved_assignees is all-None).
        let assignees: Vec<Option<String>> = vec![None; n];
        let first = tasks
            .respond_plan_confirm(
                "default", "operator", &brief, &confirm, "operator", true, &assignees,
            )
            .unwrap();
        assert_eq!(first.outcome, "approved");
        assert_eq!(first.created.len(), n);
        assert_eq!(tasks.list_subbriefs(&brief).unwrap().len(), n);

        // A duplicate accept is idempotent — the SAME ids, never doubled.
        let again = tasks
            .respond_plan_confirm(
                "default", "operator", &brief, &confirm, "operator", true, &assignees,
            )
            .unwrap();
        assert_eq!(again.outcome, "already_approved");
        assert_eq!(again.created, first.created);
        assert_eq!(tasks.list_subbriefs(&brief).unwrap().len(), n);
    }

    // PP7) With the switch OFF (the default) the tick authors NO plan package — the
    //      idle candidate is recorded exactly as before.
    #[test]
    fn plan_package_off_by_default_authors_nothing() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief) = idle_single_brief_mandate(&agents, &spine, &tasks, "default", "pp7");

        // The standard `tick` helper leaves every LLM switch (incl. plan-package) off.
        let recs = tick(&agents, &spine, &tasks, &reg, 1, Some("default"));
        assert!(
            recs.iter()
                .all(|r| !(r.target_id == m && r.action == "plan_package")),
            "no plan-package action when the switch is off"
        );
        assert_eq!(open_interactions(&tasks, &brief), (0, 0));
        assert!(tasks.latest_dossier(&brief, "plan").unwrap().is_none());
    }

    // ── PRIME ACTIVE PLANNER TRIGGER v2 (`before_execute`) ──────────────────
    // The v2 layer: when the trigger is `before_execute`, open a *proposed*
    // decomposition plan package BEFORE starting a lone eligible un-decomposed
    // Brief and HOLD the raw start, leaving the confirm OPEN for a human. The
    // candidate is a `ready_bare_mandate` (a lone ready leaf Brief at
    // `ready_to_start` that the v1 tail would never touch — it starts before the
    // tail). Self-approval / agent assignment / child creation stay out of scope.

    /// The lone leaf Brief under a [`ready_bare_mandate`] (its only Brief).
    fn lone_brief(tasks: &Arc<TaskStore>, mandate: &str) -> String {
        let briefs = tasks.list_briefs_by_mandate(mandate, 10).unwrap();
        assert_eq!(briefs.len(), 1, "ready_bare_mandate has exactly one Brief");
        briefs[0].task_id.clone()
    }

    // PPT1) The DEFAULT (`Tail`) trigger — what a blank / unknown configured value
    //       falls back to — does NOT preempt: a ready leaf Brief STARTS as before
    //       and no plan package is authored.
    #[tokio::test]
    async fn plan_package_tail_trigger_does_not_preempt_a_ready_start() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = ready_bare_mandate(&agents, &spine, &tasks, "default");
        let brief = lone_brief(&tasks, &m);
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        // Plan-package switch ON, but trigger = Tail (the blank/unknown fallback).
        let recs = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::Tail,
        );
        // The ready work started (the v1 governed start), NOT a plan package.
        let started = recs
            .iter()
            .find(|r| r.target_id == m && r.outcome == "started")
            .expect("the lone ready Brief is started under the tail trigger");
        assert_eq!(started.phase, "ready_to_start");
        assert!(
            recs.iter().all(|r| r.action != "plan_package"),
            "tail trigger authors no plan package when the Brief is ready to start"
        );
        assert_eq!(open_interactions(&tasks, &brief), (0, 0));
        assert!(tasks.latest_dossier(&brief, "plan").unwrap().is_none());
    }

    // PPT2) `before_execute` PREEMPTS the raw start: it opens EXACTLY ONE plan
    //       package on the lone ready Brief, leaves the confirm OPEN (no
    //       self-approval), and HOLDS the start (no run begins).
    #[test]
    fn plan_package_before_execute_preempts_a_ready_start() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = ready_bare_mandate(&agents, &spine, &tasks, "default");
        let brief = lone_brief(&tasks, &m);
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        let recs = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        let rec = recs
            .iter()
            .find(|r| r.target_id == m && r.action == "plan_package")
            .expect("before_execute opens a plan package instead of starting");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.phase, "ready_to_start");
        assert_eq!(rec.plan_package_ai_mode.as_deref(), Some("llm_used"));
        assert_eq!(rec.plan_package_trigger.as_deref(), Some("before_execute"));
        assert_eq!(rec.child_count, Some(2));
        assert!(rec.confirm_id.is_some());
        // The package exists with an OPEN confirm (no self-approval), and NO run
        // started — the raw start is HELD for a human decision.
        assert_eq!(open_interactions(&tasks, &brief), (1, 1));
        assert!(tasks.list_subbriefs(&brief).unwrap().is_empty());
        assert!(
            recs.iter().all(|r| r.outcome != "started"),
            "the raw Brief start is held while the decomposition awaits approval"
        );
        assert!(
            tasks
                .list_runs_for_tenant("default", 100)
                .unwrap()
                .is_empty(),
            "no Shift run begins under before_execute preemption"
        );
    }

    // PPT3) A SECOND before_execute tick neither duplicates the package nor starts
    //       the raw Brief — while a package is PENDING approval the start stays
    //       held, and the skip consumes no extra budget (records `skipped`).
    #[test]
    fn plan_package_before_execute_second_tick_holds_without_duplicate() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = ready_bare_mandate(&agents, &spine, &tasks, "default");
        let brief = lone_brief(&tasks, &m);
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        let _ = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        assert_eq!(open_interactions(&tasks, &brief), (1, 1));

        let recs2 = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        let rec2 = recs2
            .iter()
            .find(|r| r.target_id == m && r.action == "plan_package")
            .expect("the re-tick reports the pending package");
        assert_eq!(rec2.outcome, "skipped");
        assert!(rec2.reason.contains("await"), "got: {}", rec2.reason);
        // Still exactly one package, still no run, still no children — the pending
        // decomposition holds the start across ticks.
        assert_eq!(open_interactions(&tasks, &brief), (1, 1));
        assert!(
            recs2.iter().all(|r| r.outcome != "started"),
            "a pending package keeps holding the raw start on later ticks"
        );
        assert!(
            tasks
                .list_runs_for_tenant("default", 100)
                .unwrap()
                .is_empty()
        );
    }

    // PPT4) An already-existing `plan` Dossier (e.g. human-authored) prevents
    //       preemptive authoring (no duplicate / clobber) — and, since it is NOT a
    //       pending package, the active planner lets the already-planned Brief
    //       proceed to its normal start rather than stalling forever.
    #[tokio::test]
    async fn plan_package_before_execute_respects_existing_plan_dossier() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = ready_bare_mandate(&agents, &spine, &tasks, "default");
        let brief = lone_brief(&tasks, &m);
        // A human writes a plan Dossier first (no decomposition / open package).
        tasks
            .add_dossier(&brief, "plan", "Human plan", "do it by hand")
            .unwrap();
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        let recs = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        // No plan package was authored, and the human plan is intact.
        assert!(
            recs.iter().all(|r| r.action != "plan_package"),
            "an existing plan Dossier prevents preemptive package authoring"
        );
        assert_eq!(open_interactions(&tasks, &brief), (0, 0));
        assert_eq!(
            tasks.latest_dossier(&brief, "plan").unwrap().unwrap().title,
            "Human plan"
        );
        // The already-planned Brief is NOT stalled — it proceeds to its normal start.
        assert!(
            recs.iter()
                .any(|r| r.target_id == m && r.outcome == "started"),
            "an already-planned Brief still starts under before_execute"
        );
    }

    // PPT5) Tenant isolation — a before_execute tick scoped to another Guild never
    //       preempts (or starts) this Guild's Brief.
    #[test]
    fn plan_package_before_execute_tenant_isolation() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = ready_bare_mandate(&agents, &spine, &tasks, "guild-a");
        let brief = lone_brief(&tasks, &m);
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        // Drive a DIFFERENT Guild — guild-a's Brief must be untouched.
        let _ = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("guild-b"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        assert_eq!(open_interactions(&tasks, &brief), (0, 0));
        assert!(tasks.latest_dossier(&brief, "plan").unwrap().is_none());
        assert!(
            tasks
                .list_runs_for_tenant("guild-a", 100)
                .unwrap()
                .is_empty()
        );
    }

    // PPT6) The master opt-in still governs: with `RELIX_PRIME_LLM_PLAN_PACKAGE`
    //       OFF, a `before_execute` trigger authors NOTHING and the Brief starts
    //       exactly as legacy (byte-for-byte) — the trigger is inert.
    #[tokio::test]
    async fn plan_package_before_execute_inert_when_master_switch_off() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = ready_bare_mandate(&agents, &spine, &tasks, "default");
        let brief = lone_brief(&tasks, &m);
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        // Master plan-package switch OFF, but trigger = before_execute.
        let recs = autonomous_prime_tick(
            &agents,
            &spine,
            &tasks,
            &reg,
            None,
            0,
            5,
            Some("default"),
            "echo",
            Some(&decider),
            false,
            false,
            false,
            false,
            false,
            PrimePlanPackageTrigger::BeforeExecute,
        )
        .unwrap();
        assert!(
            recs.iter().all(|r| r.action != "plan_package"),
            "no plan package is authored when the master switch is off"
        );
        assert_eq!(open_interactions(&tasks, &brief), (0, 0));
        assert!(tasks.latest_dossier(&brief, "plan").unwrap().is_none());
        // Legacy behaviour: the ready Brief starts.
        assert!(
            recs.iter()
                .any(|r| r.target_id == m && r.outcome == "started"),
            "with the master switch off the Brief starts exactly as before"
        );
    }

    // PPT7) Malformed / no-decider output is SAFE on the before_execute path too:
    //       it degrades to a deterministic package (fallback / unavailable), still
    //       leaving the confirm open and holding the start.
    #[test]
    fn plan_package_before_execute_falls_back_safely() {
        // Malformed JSON → deterministic fallback, one valid package, start held.
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = ready_bare_mandate(&agents, &spine, &tasks, "default");
        let brief = lone_brief(&tasks, &m);
        let bad = ScriptedDecider {
            reply: Ok("not json at all".to_string()),
        };
        let recs = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&bad),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        let rec = recs
            .iter()
            .find(|r| r.target_id == m && r.action == "plan_package")
            .unwrap();
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.plan_package_ai_mode.as_deref(), Some("fallback"));
        assert_eq!(rec.plan_package_trigger.as_deref(), Some("before_execute"));
        assert_eq!(rec.child_count, Some(3)); // the deterministic 3-step chain
        assert_eq!(open_interactions(&tasks, &brief), (1, 1));
        assert!(
            tasks
                .list_runs_for_tenant("default", 100)
                .unwrap()
                .is_empty()
        );

        // No decider wired → unavailable, still a deterministic package, start held.
        let (agents2, spine2, tasks2) = stores();
        let tasks2 = Arc::new(tasks2);
        let m2 = ready_bare_mandate(&agents2, &spine2, &tasks2, "default");
        let brief2 = lone_brief(&tasks2, &m2);
        let recs2 = tick_pp_trig(
            &agents2,
            &spine2,
            &tasks2,
            &reg,
            5,
            Some("default"),
            None,
            PrimePlanPackageTrigger::BeforeExecute,
        );
        let rec2 = recs2
            .iter()
            .find(|r| r.target_id == m2 && r.action == "plan_package")
            .unwrap();
        assert_eq!(rec2.outcome, "advanced");
        assert_eq!(rec2.plan_package_ai_mode.as_deref(), Some("unavailable"));
        assert_eq!(open_interactions(&tasks2, &brief2), (1, 1));
        assert!(
            tasks2
                .list_runs_for_tenant("default", 100)
                .unwrap()
                .is_empty()
        );
    }

    // ── PRIME PLAN-PACKAGE APPROVAL — STANDING AUTHORITY v1 ─────────────────
    // The next slice: with an explicit `prime.plan_package.approve` standing
    // grant, the loop ACCEPTS/materializes a plan package it ITSELF authored,
    // through the existing plan-confirm path + exactly-once decomposition ledger.
    // No grant → the confirm stays open (the pending package keeps holding the
    // start). Prime-authored packages only; tenant-scoped; idempotent.

    /// Open a plan package on `brief` authored by `author`, with two unassigned
    /// children. `AUTONOMOUS_PRIME_AUTHORITY` makes it a Prime-authored package
    /// the approval gate may accept; any other author makes it a human/other-actor
    /// package the gate must refuse.
    fn open_package_as(
        tasks: &Arc<TaskStore>,
        brief: &str,
        author: &str,
    ) -> crate::nodes::coordinator::brief::PlanPackage {
        use crate::nodes::coordinator::brief::ChildSpec;
        let children = vec![
            ChildSpec {
                title: "Wire the form".into(),
                priority: Some("high".into()),
                after: None,
                assignee_agent_id: None,
                assignee_role: None,
            },
            ChildSpec {
                title: "Hook up auth".into(),
                priority: None,
                after: Some(0),
                assignee_agent_id: None,
                assignee_role: None,
            },
        ];
        tasks
            .open_plan_package(
                brief,
                author,
                "Login plan",
                "# Plan\n\nDecompose the login work.",
                "Decompose the login page",
                &children,
                "Approve this plan and create the proposed task(s)?",
            )
            .unwrap()
    }

    // PPA1) No standing grant: a Prime-authored open package is NOT approved — the
    //       confirm stays open, no children materialize, and the grant is untouched.
    #[test]
    fn plan_package_approve_without_standing_leaves_confirm_open() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (_m, brief) = idle_single_brief_mandate(&agents, &spine, &tasks, "default", "ppa1");
        let pkg = open_package_as(&tasks, &brief, AUTONOMOUS_PRIME_AUTHORITY);

        // No grant. A tick must not approve the package.
        let recs = tick_pp(&agents, &spine, &tasks, &reg, 5, Some("default"), None);
        assert!(
            recs.iter().all(|r| r.action != "plan_package_approve"),
            "no approval without a standing grant"
        );
        // Confirm still open, no children materialized.
        assert_eq!(open_interactions(&tasks, &brief), (1, 1));
        assert!(tasks.list_subbriefs(&brief).unwrap().is_empty());
        let ix = tasks.list_interactions(&brief).unwrap();
        assert!(
            ix.iter()
                .any(|i| i.interaction_id == pkg.confirm_id && i.status == "open"),
            "the Prime confirm is left OPEN"
        );
    }

    // PPA2) With the `prime.plan_package.approve` standing grant: the open
    //       Prime-authored package is accepted through the EXISTING plan-confirm
    //       path + ledger — children materialize, the confirm resolves, and the
    //       bounded (max_calls=1) grant is consumed exactly once.
    #[test]
    fn plan_package_approve_with_standing_materializes_through_ledger() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief) = idle_single_brief_mandate(&agents, &spine, &tasks, "default", "ppa2");
        let pkg = open_package_as(&tasks, &brief, AUTONOMOUS_PRIME_AUTHORITY);
        let _grant = grant_standing(&agents, "default", CATEGORY_PLAN_PACKAGE_APPROVE, Some(1));

        let recs = tick_pp(&agents, &spine, &tasks, &reg, 5, Some("default"), None);
        let rec = recs
            .iter()
            .find(|r| r.target_id == m && r.action == "plan_package_approve")
            .expect("the pending Prime package is approved with the standing grant");
        assert_eq!(rec.outcome, "advanced");
        assert_eq!(rec.confirm_id.as_deref(), Some(pkg.confirm_id.as_str()));
        assert_eq!(rec.child_count, Some(2));
        // Children materialized through the ledger; the confirm is resolved.
        assert_eq!(tasks.list_subbriefs(&brief).unwrap().len(), 2);
        let ix = tasks.list_interactions(&brief).unwrap();
        assert!(
            ix.iter()
                .any(|i| i.interaction_id == pkg.confirm_id && i.status == "resolved"),
            "the Prime confirm is resolved on approval"
        );
        // The bounded grant (max_calls=1) is now exhausted — consumed exactly once.
        assert!(
            !standing_active(&agents, "default", CATEGORY_PLAN_PACKAGE_APPROVE, 0),
            "a bounded grant is consumed once on approval"
        );
    }

    // PPA3) Re-tick idempotency: after approval the confirm is resolved, so a
    //       second tick neither materializes duplicate children nor consumes a
    //       second grant call (max_calls=2 → still one left).
    #[test]
    fn plan_package_approve_is_idempotent_on_retick() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief) = idle_single_brief_mandate(&agents, &spine, &tasks, "default", "ppa3");
        let _pkg = open_package_as(&tasks, &brief, AUTONOMOUS_PRIME_AUTHORITY);
        let _grant = grant_standing(&agents, "default", CATEGORY_PLAN_PACKAGE_APPROVE, Some(2));

        let recs1 = tick_pp(&agents, &spine, &tasks, &reg, 5, Some("default"), None);
        assert!(
            recs1
                .iter()
                .any(|r| r.target_id == m && r.action == "plan_package_approve"),
            "first tick approves"
        );
        assert_eq!(tasks.list_subbriefs(&brief).unwrap().len(), 2);

        // Re-tick: the confirm is already resolved → nothing to approve.
        let recs2 = tick_pp(&agents, &spine, &tasks, &reg, 5, Some("default"), None);
        assert!(
            recs2.iter().all(|r| r.action != "plan_package_approve"),
            "no second approval on the re-tick"
        );
        // No duplicate children.
        assert_eq!(tasks.list_subbriefs(&brief).unwrap().len(), 2);
        // The grant still has a call left — only ONE was consumed across two ticks.
        assert!(
            standing_active(&agents, "default", CATEGORY_PLAN_PACKAGE_APPROVE, 0),
            "only one grant call is consumed across the two ticks"
        );
    }

    // PPA4) Tenant isolation: a grant in Guild A cannot approve a Guild B package.
    //       The package lives in `guild-b` (no grant there); the grant is in
    //       `guild-a`. Driving `guild-b` must not approve it.
    #[test]
    fn plan_package_approve_is_tenant_isolated() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (_m, brief) = idle_single_brief_mandate(&agents, &spine, &tasks, "guild-b", "ppa4");
        let pkg = open_package_as(&tasks, &brief, AUTONOMOUS_PRIME_AUTHORITY);
        // Grant the approval authority in a DIFFERENT Guild.
        let _grant = grant_standing(&agents, "guild-a", CATEGORY_PLAN_PACKAGE_APPROVE, Some(5));

        // Drive guild-b (where the package lives) — it has no grant of its own.
        let recs = tick_pp(&agents, &spine, &tasks, &reg, 5, Some("guild-b"), None);
        assert!(
            recs.iter().all(|r| r.action != "plan_package_approve"),
            "a guild-a grant must not approve a guild-b package"
        );
        assert!(tasks.list_subbriefs(&brief).unwrap().is_empty());
        let ix = tasks.list_interactions(&brief).unwrap();
        assert!(
            ix.iter()
                .any(|i| i.interaction_id == pkg.confirm_id && i.status == "open"),
            "the cross-Guild package stays open"
        );
    }

    // PPA5) A human/other-actor package is NEVER auto-approved — even with the
    //       grant. This authority is for Prime-authored packages only.
    #[test]
    fn plan_package_approve_refuses_human_authored_package() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (_m, brief) = idle_single_brief_mandate(&agents, &spine, &tasks, "default", "ppa5");
        // A human (operator) opens the package — NOT the autonomous Prime authority.
        let pkg = open_package_as(&tasks, &brief, "operator");
        let _grant = grant_standing(&agents, "default", CATEGORY_PLAN_PACKAGE_APPROVE, Some(5));

        let recs = tick_pp(&agents, &spine, &tasks, &reg, 5, Some("default"), None);
        assert!(
            recs.iter().all(|r| r.action != "plan_package_approve"),
            "a human-authored package is never auto-approved"
        );
        assert!(tasks.list_subbriefs(&brief).unwrap().is_empty());
        let ix = tasks.list_interactions(&brief).unwrap();
        assert!(
            ix.iter()
                .any(|i| i.interaction_id == pkg.confirm_id && i.status == "open"),
            "the human package stays open"
        );
    }

    // PPA6) Before-execute pipeline integration: tick 1 OPENS the package (trigger
    //       before_execute) and holds the start; tick 2 — with the standing grant —
    //       APPROVES/materializes it through the ledger BEFORE any new open / raw
    //       start; tick 3 proceeds without duplicating the package or re-approving.
    #[test]
    fn plan_package_before_execute_then_standing_approval_then_proceed() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = ready_bare_mandate(&agents, &spine, &tasks, "default");
        let brief = lone_brief(&tasks, &m);
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };

        // Tick 1 — before_execute opens the package and HOLDS the start (no grant).
        let recs1 = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        let open = recs1
            .iter()
            .find(|r| r.target_id == m && r.action == "plan_package")
            .expect("tick 1 opens the package");
        assert_eq!(open.outcome, "advanced");
        let confirm = open.confirm_id.clone().unwrap();
        assert_eq!(open_interactions(&tasks, &brief), (1, 1));
        assert!(tasks.list_subbriefs(&brief).unwrap().is_empty());

        // Grant the approval authority, then Tick 2 — the pending Prime package is
        // accepted through the ledger before any new open / raw start.
        let _grant = grant_standing(&agents, "default", CATEGORY_PLAN_PACKAGE_APPROVE, Some(1));
        let recs2 = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        let appr = recs2
            .iter()
            .find(|r| r.target_id == m && r.action == "plan_package_approve")
            .expect("tick 2 approves the pending package with the grant");
        assert_eq!(appr.outcome, "advanced");
        assert_eq!(appr.confirm_id.as_deref(), Some(confirm.as_str()));
        assert_eq!(tasks.list_subbriefs(&brief).unwrap().len(), 2);
        // Confirm resolved, no open package left.
        assert_eq!(open_interactions(&tasks, &brief), (0, 0));

        // Tick 3 — the package is materialized; the candidate proceeds without
        // opening a duplicate package or re-approving.
        let recs3 = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        assert!(
            recs3.iter().all(|r| r.action != "plan_package_approve"),
            "no second approval"
        );
        assert!(
            recs3.iter().all(|r| r.action != "plan_package"),
            "no duplicate package is opened after materialization"
        );
        // Still exactly the two children — no duplication.
        assert_eq!(tasks.list_subbriefs(&brief).unwrap().len(), 2);
    }

    /// Count the Mandate's autonomous Chronicle events of one type, read from the
    /// SAME stable anchor Brief the driver writes them to
    /// ([`mandate_chronicle_anchor`]).
    fn chronicle_count(tasks: &TaskStore, mandate: &str, event_type: &str) -> usize {
        let anchor = mandate_chronicle_anchor(tasks, mandate).expect("mandate has a Brief");
        tasks
            .list_events_after(&anchor, 0, 500)
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == event_type)
            .count()
    }

    // E2E-1) Release-grade autonomous Prime END-TO-END smoke: the new active-planner
    //        chain drives ALL THE WAY to real execution on the safe `echo` Rig
    //        through the existing governed APIs — and is exactly-once / idempotent
    //        at every hop. This is NOT an isolated helper test: it runs the real
    //        `autonomous_prime_tick` repeatedly with a bounded `max`, the
    //        `before_execute` active-planner trigger, master plan-package authoring
    //        on, and the `prime.plan_package.approve` standing authority, asserting
    //        the whole chain is real and governed:
    //          1. tick OPENS a plan package BEFORE the raw start and HOLDS it;
    //          2. a later tick ACCEPTS/materializes the Prime-authored package
    //             through the EXISTING confirm/decomposition ledger — children exist
    //             exactly once, the bounded grant is consumed exactly once;
    //          3. a re-tick neither duplicates the package, the approval, nor the
    //             children, and consumes no second grant;
    //          4. with the `prime.brief.assign_decomposed` standing grant, a tick
    //             AUTONOMOUSLY assigns the unassigned Prime-decomposed children to
    //             the parent Brief's own active echo assignee (no human assignment),
    //             through the existing assignee primitive — the bounded grant is
    //             consumed exactly once and a re-tick neither reassigns nor consumes
    //             a second grant;
    //          5. once the children are assigned + ready the loop STARTS them as
    //             durable Shifts on the `echo` Rig (heartbeat-trigger runs);
    //          6. the loop then HONESTLY STOPS at the next governance gate
    //             (run review — no `prime.run.review_accept` authority), opening no
    //             duplicate runs.
    //        The Chronicle records each real action (`plan_package`,
    //        `plan_package_approve`, the child assignment, the Mandate start) and is
    //        NOT spammed.
    #[tokio::test]
    async fn prime_autonomy_e2e_plan_package_to_execution() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = ready_bare_mandate(&agents, &spine, &tasks, "default");
        let brief = lone_brief(&tasks, &m);
        // The active, runnable echo Operative the lone leaf is already assigned to.
        let eng = tasks
            .brief_card(&brief)
            .unwrap()
            .unwrap()
            .assignee_agent_id
            .unwrap();
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        // Enable ONLY the two standing authorities the chain needs, each bounded to a
        // SINGLE call: plan-package approval (one materialization) and decomposed-child
        // assignment (one assignment batch). Orchestration / start auto-advance through
        // the shared guarded pipeline; no run-review / apply grant is given, so the loop
        // still honours that human gate at the tail, not bypassed.
        let _grant = grant_standing(&agents, "default", CATEGORY_PLAN_PACKAGE_APPROVE, Some(1));
        let _assign_grant = grant_standing(&agents, "default", CATEGORY_ASSIGN_DECOMPOSED, Some(1));

        // ── Tick 1 — before_execute OPENS a *proposed* package and HOLDS the raw
        //    start. No children, no run: the undecomposed leaf is NOT started.
        let r1 = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        let open = r1
            .iter()
            .find(|r| r.target_id == m && r.action == "plan_package")
            .expect("tick 1 opens the plan package before the raw start");
        assert_eq!(open.outcome, "advanced");
        assert_eq!(open.phase, "ready_to_start", "the start is preempted");
        let confirm = open.confirm_id.clone().expect("the package has a confirm");
        assert_eq!(open_interactions(&tasks, &brief), (1, 1));
        assert!(
            tasks.list_subbriefs(&brief).unwrap().is_empty(),
            "no children before approval"
        );
        assert!(
            tasks
                .list_runs_for_tenant("default", 100)
                .unwrap()
                .is_empty(),
            "the raw leaf is HELD, not started, while the package is pending"
        );

        // ── Tick 2 — the pending Prime-authored package is ACCEPTED through the
        //    existing plan-confirm path + exactly-once decomposition ledger.
        let r2 = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        let appr = r2
            .iter()
            .find(|r| r.target_id == m && r.action == "plan_package_approve")
            .expect("tick 2 approves the pending package with the standing grant");
        assert_eq!(appr.outcome, "advanced");
        assert_eq!(appr.confirm_id.as_deref(), Some(confirm.as_str()));
        assert_eq!(appr.child_count, Some(2));
        // Children materialized through the ledger; the confirm is resolved.
        assert_eq!(tasks.list_subbriefs(&brief).unwrap().len(), 2);
        assert_eq!(open_interactions(&tasks, &brief), (0, 0));
        // The bounded (max_calls=1) grant is consumed exactly once.
        assert!(
            !standing_active(&agents, "default", CATEGORY_PLAN_PACKAGE_APPROVE, 0),
            "the bounded grant is consumed exactly once on materialization"
        );

        // ── Tick 3 — AUTONOMOUS ASSIGNMENT gate (no human assigns). With the
        //    `prime.brief.assign_decomposed` grant, the loop assigns the unassigned
        //    Prime-decomposed children to the parent Brief's OWN active echo
        //    assignee through the existing assignee primitive — before the
        //    orchestration no-op. It does NOT start them this tick (one governed
        //    step per candidate per tick). This is also a re-tick: no duplicate
        //    package, no second approval, no duplicate children.
        let r3 = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        assert!(
            r3.iter()
                .all(|r| r.action != "plan_package" && r.action != "plan_package_approve"),
            "no duplicate package or approval after materialization"
        );
        let assign = r3
            .iter()
            .find(|r| r.target_id == m && r.action == "assign_decomposed_children")
            .expect("tick 3 autonomously assigns the Prime-decomposed children");
        assert_eq!(assign.outcome, "advanced");
        assert_eq!(
            tasks.list_subbriefs(&brief).unwrap().len(),
            2,
            "still exactly two children"
        );
        // Both children are now assigned to the parent's own echo Operative —
        // Prime never picked an agent, it inherited the parent assignee.
        let kids = tasks.list_subbriefs(&brief).unwrap();
        for k in &kids {
            assert_eq!(
                tasks
                    .brief_card(k)
                    .unwrap()
                    .unwrap()
                    .assignee_agent_id
                    .as_deref(),
                Some(eng.as_str()),
                "the child inherited the parent Brief's active assignee"
            );
        }
        // The bounded (max_calls=1) assignment grant is consumed exactly once.
        assert!(
            !standing_active(&agents, "default", CATEGORY_ASSIGN_DECOMPOSED, 0),
            "the bounded assignment grant is consumed exactly once on the batch"
        );
        // No Shift has started yet — assignment is its own governed step.
        assert!(
            tasks
                .list_runs_for_tenant("default", 100)
                .unwrap()
                .is_empty(),
            "the assignment tick assigns but does not start the children"
        );

        // ── Tick 4 — the children are now assigned + ready: the loop STARTS them as
        //    durable Shifts on the `echo` Rig (heartbeat-trigger runs, not manual).
        //    This is also a re-tick proving assignment idempotency: the assignment
        //    grant is NOT consumed a second time and no child is reassigned.
        let r4 = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        assert!(
            r4.iter().all(|r| r.action != "assign_decomposed_children"),
            "no second assignment after the children are already assigned"
        );
        assert!(
            !standing_active(&agents, "default", CATEGORY_ASSIGN_DECOMPOSED, 0),
            "the assignment grant stays consumed exactly once (no second consume)"
        );
        let started = r4
            .iter()
            .find(|r| r.target_id == m && r.outcome == "started")
            .expect("tick 4 starts the assigned children");
        assert_eq!(started.phase, "ready_to_start");
        assert_eq!(started.action, "start_mandate");
        let runs = tasks.list_runs_for_tenant("default", 100).unwrap();
        assert_eq!(runs.len(), 2, "exactly the two child Shifts run");
        assert!(
            runs.iter().all(|r| r.rig == "echo"),
            "the child Shifts run on the safe echo Rig"
        );
        assert!(
            runs.iter()
                .all(|r| r.trigger.as_deref() == Some("heartbeat")),
            "autonomous starts stamp a heartbeat-trigger run, not dashboard manual"
        );

        // ── Tick 5 — the completed echo Shifts now await review; with no
        //    `prime.run.review_accept` authority the loop HONESTLY STOPS at that
        //    governance gate and opens no duplicate run.
        let r5 = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        assert!(
            r5.iter()
                .any(|r| r.target_id == m && r.phase == "needs_review" && r.outcome == "blocked"),
            "the loop parks honestly at the run-review gate"
        );
        assert_eq!(
            tasks.list_runs_for_tenant("default", 100).unwrap().len(),
            2,
            "no duplicate run is opened after the children have started"
        );

        // ── Chronicle: the real actions are recorded exactly once each (on the
        //    Mandate's stable anchor Brief), and the Chronicle is not spammed.
        assert_eq!(
            chronicle_count(&tasks, &m, "prime.autonomous_plan_package"),
            1,
            "the plan-package open is Chronicled exactly once"
        );
        assert_eq!(
            chronicle_count(&tasks, &m, "prime.autonomous_plan_package_approve"),
            1,
            "the plan-package approval is Chronicled exactly once"
        );
        assert_eq!(
            chronicle_count(&tasks, &m, "prime.autonomous_assign_decomposed"),
            1,
            "the decomposed-child assignment is Chronicled exactly once"
        );
        assert_eq!(
            chronicle_count(&tasks, &m, "prime.autonomous_mandate_start"),
            1,
            "the Mandate start is Chronicled exactly once"
        );
    }

    // E2E-2) Regression for the liveness gap this smoke found: when the autonomous
    //        loop materializes a Prime-decomposed package but the children are left
    //        UNASSIGNED (Prime never assigns; no human passes the assignment gate),
    //        the Mandate sits at `needs_orchestration`. The governed
    //        `orchestrate_assign_ready` builds its own skeleton ONCE but cannot
    //        assign the decomposed children, so without the idempotent-no-op guard
    //        the loop would re-run orchestration EVERY tick — taking false
    //        `advanced` credit and spamming the Chronicle forever (a livelock). The
    //        guard makes the loop HONEST: the first orchestration advances, then
    //        every further tick is `skipped` (no action, no Chronicle event), the
    //        children stay exactly two, and the raw undecomposed leaf is never run.
    #[tokio::test]
    async fn prime_autonomy_e2e_orchestration_no_op_is_honest() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = ready_bare_mandate(&agents, &spine, &tasks, "default");
        let brief = lone_brief(&tasks, &m);
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        let _grant = grant_standing(&agents, "default", CATEGORY_PLAN_PACKAGE_APPROVE, Some(1));

        // Tick 1 opens, tick 2 materializes the two unassigned children.
        for _ in 0..2 {
            tick_pp_trig(
                &agents,
                &spine,
                &tasks,
                &reg,
                5,
                Some("default"),
                Some(&decider),
                PrimePlanPackageTrigger::BeforeExecute,
            );
        }
        assert_eq!(tasks.list_subbriefs(&brief).unwrap().len(), 2);

        // Drive several more ticks. Exactly ONE must really advance orchestration;
        // the rest must be honest no-ops.
        let mut advanced = 0usize;
        let mut skipped = 0usize;
        for _ in 0..6 {
            let recs = tick_pp_trig(
                &agents,
                &spine,
                &tasks,
                &reg,
                5,
                Some("default"),
                Some(&decider),
                PrimePlanPackageTrigger::BeforeExecute,
            );
            for r in recs
                .iter()
                .filter(|r| r.target_id == m && r.action == "orchestrate_assign_ready")
            {
                match r.outcome {
                    "advanced" => advanced += 1,
                    "skipped" => skipped += 1,
                    other => panic!("unexpected orchestration outcome: {other}"),
                }
            }
        }
        assert_eq!(advanced, 1, "orchestration does real work exactly once");
        assert!(
            skipped >= 4,
            "the rest are honest no-ops, not false advances"
        );

        // Children are untouched and the raw undecomposed leaf is never started.
        assert_eq!(tasks.list_subbriefs(&brief).unwrap().len(), 2);
        assert!(
            tasks
                .list_runs_for_tenant("default", 100)
                .unwrap()
                .is_empty(),
            "no Shift runs while the decomposed children await assignment"
        );
        // The Chronicle is not spammed: exactly one orchestration advance event.
        assert_eq!(chronicle_count(&tasks, &m, "prime.autonomous_advance"), 1);
    }

    // ── PRIME-DECOMPOSED CHILD ASSIGNMENT — STANDING AUTHORITY v1 ─────────────
    // Focused unit coverage for the narrow `prime.brief.assign_decomposed`
    // authority, on top of the E2E chain above.

    /// Drive a `ready_bare_mandate` in `tenant` through the autonomous OPEN +
    /// MATERIALIZE of a Prime-authored plan package, returning
    /// `(mandate_id, parent_brief_id, parent_assignee)` with TWO UNASSIGNED
    /// Prime-decomposed children under the parent (the parent keeps its active
    /// echo assignee). Consumes the bounded `prime.plan_package.approve` grant;
    /// creates NO assignment grant (each test decides that).
    fn prime_decomposed_unassigned(
        agents: &AgentStore,
        spine: &SpineStore,
        tasks: &Arc<TaskStore>,
        tenant: &str,
    ) -> (String, String, String) {
        let reg = echo_registry();
        let m = ready_bare_mandate(agents, spine, tasks, tenant);
        let brief = lone_brief(tasks, &m);
        let eng = tasks
            .brief_card(&brief)
            .unwrap()
            .unwrap()
            .assignee_agent_id
            .unwrap();
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        let _grant = grant_standing(agents, tenant, CATEGORY_PLAN_PACKAGE_APPROVE, Some(1));
        // Tick 1 opens the package; tick 2 materializes the two unassigned children.
        for _ in 0..2 {
            tick_pp_trig(
                agents,
                spine,
                tasks,
                &reg,
                5,
                Some(tenant),
                Some(&decider),
                PrimePlanPackageTrigger::BeforeExecute,
            );
        }
        assert_eq!(tasks.list_subbriefs(&brief).unwrap().len(), 2);
        (m, brief, eng)
    }

    fn child_assignees(tasks: &Arc<TaskStore>, parent: &str) -> Vec<Option<String>> {
        tasks
            .list_subbriefs(parent)
            .unwrap()
            .into_iter()
            .map(|k| tasks.brief_card(&k).unwrap().unwrap().assignee_agent_id)
            .collect()
    }

    // AD1) No grant: a materialized Prime-decomposed child set stays UNASSIGNED —
    //      the loop never assigns and no assignment grant is consumed (there is
    //      none); the chain parks honestly at the assignment gate.
    #[tokio::test]
    async fn assign_decomposed_no_grant_leaves_children_unassigned() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief, _eng) = prime_decomposed_unassigned(&agents, &spine, &tasks, "default");
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };

        // No `prime.brief.assign_decomposed` grant.
        let recs = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        assert!(
            recs.iter()
                .all(|r| r.action != "assign_decomposed_children"),
            "with no grant the loop never assigns Prime-decomposed children"
        );
        assert!(
            child_assignees(&tasks, &brief).iter().all(Option::is_none),
            "the children stay unassigned without the grant"
        );
        assert_eq!(
            chronicle_count(&tasks, &m, "prime.autonomous_assign_decomposed"),
            0
        );
        assert!(
            tasks
                .list_runs_for_tenant("default", 100)
                .unwrap()
                .is_empty(),
            "no Shift runs while the children await assignment"
        );
    }

    // AD2) With the grant and a parent assigned to an active echo Operative: the
    //      materialized children are assigned to the PARENT's assignee, the
    //      bounded grant is consumed exactly once, and a re-tick neither reassigns
    //      nor consumes a second grant.
    #[tokio::test]
    async fn assign_decomposed_with_grant_assigns_to_parent_assignee_once() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief, eng) = prime_decomposed_unassigned(&agents, &spine, &tasks, "default");
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        let _ag = grant_standing(&agents, "default", CATEGORY_ASSIGN_DECOMPOSED, Some(1));

        let r1 = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        let rec = r1
            .iter()
            .find(|r| r.target_id == m && r.action == "assign_decomposed_children")
            .expect("the loop autonomously assigns the Prime-decomposed children");
        assert_eq!(rec.outcome, "advanced");
        // Every child inherited the parent's OWN active assignee (no model pick).
        assert!(
            child_assignees(&tasks, &brief)
                .iter()
                .all(|a| a.as_deref() == Some(eng.as_str())),
            "each child is assigned to the parent Brief's assignee"
        );
        assert!(
            !standing_active(&agents, "default", CATEGORY_ASSIGN_DECOMPOSED, 0),
            "the bounded grant is consumed exactly once on the batch"
        );
        assert_eq!(
            chronicle_count(&tasks, &m, "prime.autonomous_assign_decomposed"),
            1
        );

        // Re-tick: no second assignment, no second consume (the children left the
        // unassigned set, so the assignment step is not even reached again).
        let r2 = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        assert!(
            r2.iter().all(|r| r.action != "assign_decomposed_children"),
            "no second assignment after the children are already assigned"
        );
        assert_eq!(
            chronicle_count(&tasks, &m, "prime.autonomous_assign_decomposed"),
            1,
            "the assignment is Chronicled exactly once"
        );
        assert!(
            child_assignees(&tasks, &brief)
                .iter()
                .all(|a| a.as_deref() == Some(eng.as_str())),
            "the re-tick does not reassign the children"
        );
    }

    // AD3) A HUMAN/other-actor decomposition is NEVER auto-assigned, even with the
    //      grant active — the authority touches Prime-authored children only, so
    //      the grant is left untouched.
    #[tokio::test]
    async fn assign_decomposed_ignores_human_authored_decomposition() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let m = ready_bare_mandate(&agents, &spine, &tasks, "default");
        let brief = lone_brief(&tasks, &m);

        // A HUMAN opens + materializes a plan package on the leaf (author "operator").
        let mk_child = |title: &str| crate::nodes::coordinator::brief::ChildSpec {
            title: title.to_string(),
            priority: None,
            after: None,
            assignee_agent_id: None,
            assignee_role: None,
        };
        let pkg = tasks
            .open_plan_package(
                &brief,
                "operator",
                "Manual plan",
                "# Do it manually",
                "split the work",
                &[mk_child("part a"), mk_child("part b")],
                "approve?",
            )
            .unwrap();
        let res = tasks
            .respond_plan_confirm(
                "default",
                "operator",
                &brief,
                &pkg.confirm_id,
                "operator",
                true,
                &[],
            )
            .unwrap();
        assert_eq!(res.created.len(), 2);
        assert_eq!(tasks.list_subbriefs(&brief).unwrap().len(), 2);

        // Even WITH the grant, Prime never assigns a human-authored decomposition.
        let _ag = grant_standing(&agents, "default", CATEGORY_ASSIGN_DECOMPOSED, Some(1));
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        let recs = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        assert!(
            recs.iter()
                .all(|r| r.action != "assign_decomposed_children"),
            "a human/other-actor decomposition is never auto-assigned"
        );
        assert!(
            child_assignees(&tasks, &brief).iter().all(Option::is_none),
            "the human-authored children stay unassigned"
        );
        assert!(
            standing_active(&agents, "default", CATEGORY_ASSIGN_DECOMPOSED, 0),
            "the grant is not consumed on a non-Prime decomposition"
        );
    }

    // AD4) Tenant isolation: an assignment grant in Guild A cannot assign Guild B's
    //      Prime-decomposed children.
    #[tokio::test]
    async fn assign_decomposed_is_tenant_isolated() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (_mb, brief_b, _eng) = prime_decomposed_unassigned(&agents, &spine, &tasks, "tb");
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };

        // Grant the assignment authority in a DIFFERENT Guild only.
        let _ag = grant_standing(&agents, "ta", CATEGORY_ASSIGN_DECOMPOSED, Some(1));

        // Tick Guild tb — its children must NOT be assigned by Guild ta's grant.
        let recs = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("tb"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        assert!(
            recs.iter()
                .all(|r| r.action != "assign_decomposed_children"),
            "a grant in another Guild cannot assign this Guild's children"
        );
        assert!(
            child_assignees(&tasks, &brief_b)
                .iter()
                .all(Option::is_none),
            "Guild tb's children stay unassigned"
        );
        assert!(
            standing_active(&agents, "ta", CATEGORY_ASSIGN_DECOMPOSED, 0),
            "Guild ta's grant is untouched"
        );
    }

    // AD5) Missing/invalid parent assignee: with the grant active but the parent
    //      Brief carrying no (usable) assignee, the step records an honest
    //      `blocked`, assigns nothing, and consumes NO grant.
    #[tokio::test]
    async fn assign_decomposed_blocked_on_missing_parent_assignee() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief, _eng) = prime_decomposed_unassigned(&agents, &spine, &tasks, "default");
        // Clear the parent Brief's assignee — there is no subject to inherit.
        tasks.set_brief_field(&brief, "assignee", "").unwrap();

        let _ag = grant_standing(&agents, "default", CATEGORY_ASSIGN_DECOMPOSED, Some(1));
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        let recs = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        let blocked = recs
            .iter()
            .find(|r| r.target_id == m && r.action == "assign_decomposed_children")
            .expect("the assignment step records an honest result");
        assert_eq!(blocked.outcome, "blocked");
        assert!(
            child_assignees(&tasks, &brief).iter().all(Option::is_none),
            "no child is assigned when there is no safe parent assignee"
        );
        assert!(
            standing_active(&agents, "default", CATEGORY_ASSIGN_DECOMPOSED, 0),
            "no grant is consumed on a no-safe-assignee block"
        );
        assert_eq!(
            chronicle_count(&tasks, &m, "prime.autonomous_assign_decomposed"),
            0
        );
    }

    // AD6) Invalid parent assignee: a parent pointed at an UNKNOWN / non-active
    //      subject (not a same-Guild active Operative with a known Rig) is not a
    //      safe subject — the step blocks honestly and consumes no grant. (The
    //      active echo crew stays in place so team readiness is unchanged and the
    //      tick still reaches the assignment step.)
    #[tokio::test]
    async fn assign_decomposed_blocked_on_invalid_parent_assignee() {
        let (agents, spine, tasks) = stores();
        let tasks = Arc::new(tasks);
        let reg = echo_registry();
        let (m, brief, _eng) = prime_decomposed_unassigned(&agents, &spine, &tasks, "default");
        // Repoint the parent at a subject that is not an active same-Guild
        // Operative — assignment must refuse to inherit it.
        tasks
            .set_brief_field(&brief, "assignee", "ghost-agent-not-real")
            .unwrap();

        let _ag = grant_standing(&agents, "default", CATEGORY_ASSIGN_DECOMPOSED, Some(1));
        let decider = ScriptedDecider {
            reply: Ok(pp_reply()),
        };
        let recs = tick_pp_trig(
            &agents,
            &spine,
            &tasks,
            &reg,
            5,
            Some("default"),
            Some(&decider),
            PrimePlanPackageTrigger::BeforeExecute,
        );
        let blocked = recs
            .iter()
            .find(|r| r.target_id == m && r.action == "assign_decomposed_children")
            .expect("the assignment step records an honest result");
        assert_eq!(blocked.outcome, "blocked");
        assert!(
            child_assignees(&tasks, &brief).iter().all(Option::is_none),
            "no child is assigned to an invalid parent assignee"
        );
        assert!(
            standing_active(&agents, "default", CATEGORY_ASSIGN_DECOMPOSED, 0),
            "no grant is consumed when the parent assignee is invalid"
        );
    }
}
