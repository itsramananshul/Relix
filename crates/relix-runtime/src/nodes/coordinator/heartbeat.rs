//! The **heartbeat / assignment loop** core (Phase 3).
//!
//! This module holds the *pure, testable* selection-and-claim step
//! of the loop, decoupled from the actual outbound dispatch (which
//! wakes an Operative to run a Brief over the AI / delegation
//! path). One tick:
//!
//!   1. read the ready Briefs (`brief.ready` — assigned, active,
//!      unblocked, unclaimed), priority-ordered;
//!   2. atomically **Claim** each for its own assignee, so exactly
//!      one tick / coordinator instance ever dispatches a given
//!      Brief (single-owner);
//!   3. return the Briefs we won — *those* are the ones to
//!      dispatch this tick.
//!
//! The caller does the heavy part (running the agent) with the
//! returned batch, then heartbeats / releases the Claim. Keeping
//! the claim core here means the loop's correctness is unit-tested
//! without standing up the outbound mesh path.

use std::sync::Arc;

use super::{CoordinatorError, TaskStore, brief};
use crate::rig::bridge::BridgeTokenStore;
use crate::rig::{Rig, RigOutcome, RigRunRequest};

/// The default lease a dispatch tick takes on a claimed Brief. The
/// dispatcher must heartbeat (`TaskStore::heartbeat_claim`) before
/// this elapses, or the Brief becomes reclaimable (so a crashed
/// dispatcher's work is picked up by the next tick).
pub const DEFAULT_DISPATCH_LEASE_SECS: i64 = 300;

/// Bridge-back methods a Rig may use during one Shift. Keep this
/// list narrow: it is the difference between "agent can report work
/// on its Brief" and "leaked token can mutate the whole company."
pub const BRIDGE_BACK_SHIFT_METHODS: &[&str] = &[
    "brief.comment",
    "brief.subbrief",
    "brief.dossier_add",
    "brief.set_snags",
    "brief.claim_holder",
    "brief.clearance_request",
];

/// What triggered a Brief execution. Manual and autonomous runs go
/// through the SAME pipeline ([`prepare_claimed_run`] → [`execute_ready`])
/// and the same `brief_runs` ledger; the trigger is the only thing that
/// distinguishes them, surfaced in the run record + dashboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunTrigger {
    /// Dashboard "Run" / `brief.run` — an operator started it.
    Manual,
    /// Autonomous heartbeat/timer dispatch.
    Heartbeat,
    /// A scheduled trigger (reserved / future).
    Scheduled,
}

impl RunTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            RunTrigger::Manual => "manual",
            RunTrigger::Heartbeat => "heartbeat",
            RunTrigger::Scheduled => "scheduled",
        }
    }
}

/// Run one selection-and-claim tick over the ready Briefs.
///
/// For each Brief ready to work, atomically claim it for its
/// assignee and collect the ones we won — the Briefs to dispatch
/// this tick. Briefs already held by a live Claim are skipped
/// (another tick / coordinator owns them). Briefs with no assignee
/// are skipped defensively (the readiness query already requires
/// one).
///
/// `batch` caps how many ready Briefs we consider; `lease_secs` is
/// the Claim lease length. Pure over the store — no outbound I/O.
pub fn claim_ready_batch(
    store: &TaskStore,
    batch: usize,
    lease_secs: i64,
) -> Result<Vec<brief::BriefCard>, CoordinatorError> {
    let ready = store.list_ready_briefs(batch)?;
    let mut claimed = Vec::with_capacity(ready.len());
    for card in ready {
        let Some(assignee) = card.assignee_agent_id.as_deref() else {
            continue;
        };
        // The Claim pointer is a `run_` id (the durable run-ledger shape), not a
        // `shift_` one, matching the live heartbeat path
        // (`claim_queued_wakeups_with_caps`) where the Claim's `execution_run_id`
        // IS the `brief_runs.run_id` — the alignment terminal-evidence adoption
        // depends on. (This pure helper is test-only; the live loop claims via the
        // wakeup queue, but it must not model the old `shift_?`-pointer split.)
        let execution_run_id = format!("run_{}", uuid::Uuid::new_v4());
        if store.claim_brief_for_run(
            &card.task_id,
            assignee,
            lease_secs,
            Some(&execution_run_id),
        )? {
            claimed.push(card);
        }
    }
    Ok(claimed)
}

/// What one dispatched Brief produced this tick.
#[derive(Clone, Debug)]
pub struct DispatchRecord {
    /// The Brief that was dispatched.
    pub brief_id: String,
    /// The Rig that ran it (empty if none resolved).
    pub rig: String,
    /// The Rig's outcome.
    pub outcome: RigOutcome,
}

/// Verdict of the per-Brief Allowance / budget admission gate
/// (relix-company-model §3.6 "Budgets" + §5.2D autonomy/budget): the
/// company operating system must not keep dispatching work when the
/// assigned Operative is over its hard budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BudgetAdmission {
    /// The Operative may run this Brief.
    Allow,
    /// The Operative (per-Operative Allowance) or the whole Guild (Guild
    /// budget) is over budget / hard-stopped. `reason` is the operator-facing
    /// explanation chronicled on the Brief; `event` is the Chronicle event type
    /// recorded on refusal and `status` the durable refused-run status — these
    /// distinguish a per-Operative Allowance stop (`brief.budget_refused` /
    /// `over_allowance`) from a Guild-budget stop (`guild.budget_refused` /
    /// `over_guild_budget`) in run history and the Action Center.
    Refuse {
        reason: String,
        event: &'static str,
        status: &'static str,
    },
}

/// Chronicle event + refused-run status for a **per-Operative** Allowance stop.
pub const OPERATIVE_BUDGET_EVENT: &str = "brief.budget_refused";
pub const OPERATIVE_BUDGET_STATUS: &str = "over_allowance";
/// Chronicle event + refused-run status for a **Guild-level** budget stop
/// (relix-company-model §6.6 / §3.6 — the Guild ceiling binds autonomous spend).
pub const GUILD_BUDGET_EVENT: &str = "guild.budget_refused";
pub const GUILD_BUDGET_STATUS: &str = "over_guild_budget";

/// One US cent expressed in micro-USD (the metrics cost unit).
pub const MICROS_PER_CENT: u64 = 10_000;

/// Milliseconds in one day.
const MS_PER_DAY: i64 = 86_400_000;

/// Boundaries of the monthly **Allowance window** — the calendar period the
/// autonomous per-Operative Allowance and Guild-budget hard-stops bill against
/// (relix-company-model §6 "execution / Budgets" + §6.6 "Cost rollup &
/// attribution"; lexicon: "Allowance").
///
/// The window is the **current calendar month in UTC**:
/// - `start_ms` — the first instant of the month (00:00:00.000 UTC),
///   **inclusive**, matching `MetricsQuery::cost_since`'s `timestamp_ms >= since`
///   bound exactly;
/// - `cutoff_ms` — the upper edge of month-to-date spend (= the `now_ms` passed
///   in);
/// - `resets_at_ms` — the first instant of the **next** month (the reset
///   boundary). There is no stored counter to clear: month-to-date spend is
///   always re-summed from the live `start_ms`, so a new month is a fresh window
///   **by construction** — `resets_at_ms` is the bookkeeping value the operator
///   surface can show as "resets at …".
///
/// **UTC is deliberate and fixed.** The mesh carries no per-Guild billing
/// timezone, so a single stable zone keeps the dispatch gate, the Action Center
/// live-spend feed, and tests in exact agreement. If a per-Guild billing
/// timezone is ever introduced, it changes only this one function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AllowanceWindow {
    /// First instant of the current calendar month, unix-ms, UTC, inclusive.
    pub start_ms: i64,
    /// Month-to-date cutoff (the `now_ms` the window was computed for), unix-ms.
    pub cutoff_ms: i64,
    /// First instant of the next calendar month, unix-ms, UTC — the reset edge.
    pub resets_at_ms: i64,
}

/// THE canonical Allowance window for a wall-clock `now_ms` (unix-ms).
///
/// Every Allowance / Guild-budget spend read MUST derive its window start from
/// here — the autonomous dispatch gate ([`dispatch_budget_admits`]) and the
/// Action Center's live-spend seam (`MetricsSpendSource::current_month`) both do,
/// so the gate and the feed can never disagree by computing the window two ways.
pub fn allowance_window(now_ms: i64) -> AllowanceWindow {
    let (y, m, _d) = civil_from_days(now_ms.div_euclid(MS_PER_DAY));
    let start_day = days_from_civil(y, m, 1);
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    let reset_day = days_from_civil(ny, nm, 1);
    AllowanceWindow {
        start_ms: start_day * MS_PER_DAY,
        cutoff_ms: now_ms,
        resets_at_ms: reset_day * MS_PER_DAY,
    }
}

/// Days-since-Unix-epoch → `(year, month, day)`, Howard Hinnant's branch-free
/// `civil_from_days` (proleptic Gregorian, zero-dependency, exact). `month` is
/// 1–12, `day` is 1–31.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d)
}

/// `(year, month, day)` → days-since-Unix-epoch, Howard Hinnant's
/// `days_from_civil` (the inverse of [`civil_from_days`]). `month` is 1–12.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let m = m as i64;
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Pure per-Operative monthly Allowance verdict.
///
/// - `allowance_cents`: the Operative's configured monthly cap
///   (`AgentProfile.monthly_allowance_cents`); `None` = no per-agent
///   Allowance, so this gate allows.
/// - `spend_micros`: the Operative's spend over the window, in
///   micro-USD (from the metrics ledger).
///
/// A cap of `0` (or negative) is an explicit **hard-stop** — the
/// Operative is budgeted to nothing and must not run. A positive cap
/// refuses once spend reaches it. (1 cent = [`MICROS_PER_CENT`]
/// micro-USD.)
pub fn allowance_admits(allowance_cents: Option<i64>, spend_micros: u64) -> BudgetAdmission {
    match allowance_cents {
        None => BudgetAdmission::Allow,
        Some(c) if c <= 0 => BudgetAdmission::Refuse {
            reason: "allowance=0 (hard-stopped)".to_string(),
            event: OPERATIVE_BUDGET_EVENT,
            status: OPERATIVE_BUDGET_STATUS,
        },
        Some(c) => {
            let cap_micros = (c as u64).saturating_mul(MICROS_PER_CENT);
            if spend_micros >= cap_micros {
                BudgetAdmission::Refuse {
                    reason: format!(
                        "over monthly allowance (used {spend_micros}u >= cap {cap_micros}u)"
                    ),
                    event: OPERATIVE_BUDGET_EVENT,
                    status: OPERATIVE_BUDGET_STATUS,
                }
            } else {
                BudgetAdmission::Allow
            }
        }
    }
}

/// Pure **Guild-level** monthly budget verdict for the AUTONOMOUS dispatch path
/// (relix-company-model §6.6 "Cost rollup & attribution" + §3.6 "Budgets" + §5.4
/// the Board sets budgets). The per-Operative gate ([`allowance_admits`]) bounds
/// ONE Operative; this gate bounds the WHOLE Guild's autonomous spend so a fleet
/// of individually-in-budget Operatives can't collectively blow past the company
/// ceiling. It is **additive** — the dispatch gate applies it only after the
/// per-Operative check allows.
///
/// - `budget_cents`: the Guild's configured monthly budget
///   (`Guild.monthly_allowance_cents`). `None` or `<= 0` = **no Guild cap set**,
///   so this gate allows — matching the Action Center, which only surfaces a
///   Guild budget signal when the budget is positive (a `0` here means "unset",
///   NOT a company-wide hard-stop; that deliberately differs from the
///   per-Operative `0` = hard-stop, because the Guild budget is an Option whose
///   "unset" sentinel is `None`/`0`).
/// - `guild_spend_micros`: the Guild's month-to-date spend in micro-USD — the
///   SUM of THIS Guild's active Operatives' `cost_since` over the canonical
///   [`allowance_window`] (current UTC calendar month; never a cross-tenant
///   `cost_since(None, …)`), the exact figure the Action Center's
///   `company_spend_item` reports.
///
/// A positive budget refuses once Guild spend reaches it (`>=`), mirroring the
/// per-Operative `over_allowance` threshold. Binds AUTONOMOUS dispatch only — a
/// manual operator `brief.run` / `prime.start` never passes through this gate
/// (the Board is sovereign).
pub fn guild_allowance_admits(
    budget_cents: Option<i64>,
    guild_spend_micros: u64,
) -> BudgetAdmission {
    match budget_cents {
        None => BudgetAdmission::Allow,
        Some(c) if c <= 0 => BudgetAdmission::Allow,
        Some(c) => {
            let cap_micros = (c as u64).saturating_mul(MICROS_PER_CENT);
            if guild_spend_micros >= cap_micros {
                BudgetAdmission::Refuse {
                    reason: format!(
                        "over Guild budget (Guild used {guild_spend_micros}u >= cap {cap_micros}u)"
                    ),
                    event: GUILD_BUDGET_EVENT,
                    status: GUILD_BUDGET_STATUS,
                }
            } else {
                BudgetAdmission::Allow
            }
        }
    }
}

/// THE canonical **Guild month-to-date spend** in micro-USD: the SUM of
/// `tenant`'s active Operatives' recorded run cost (`MetricsQuery::cost_since`)
/// since `since_ms` (the canonical [`allowance_window`] start). This is the EXACT
/// figure + window the autonomous Guild-budget hard-stop
/// ([`dispatch_budget_admits`]) enforces and the Action Center's
/// `company_spend_item` reports — extracted into one helper so the dispatch gate
/// and any operator surface (the dashboard Costs page's `guild.spend` route) can
/// never disagree by summing two different ways.
///
/// **Tenant-safe by construction:** sums ONLY `tenant`'s own active roster
/// (`list_active_for_tenant`), never a cross-tenant `cost_since(None, …)`, so
/// another Guild's spend can never enter this total. A per-agent ledger read
/// error contributes `0` (mirrors the gate — a transient metrics hiccup never
/// fabricates spend, and `saturating_add` can never overflow).
pub fn guild_spend_micros(
    agent_store: &crate::nodes::coordinator::agent::store::AgentStore,
    metrics: &crate::metrics::MetricsQuery,
    tenant: &str,
    since_ms: i64,
) -> u64 {
    let mut guild_spend: u64 = 0;
    if let Ok(actives) = agent_store.list_active_for_tenant(tenant) {
        for a in &actives {
            if let Ok(used) = metrics.cost_since(Some(&a.agent_id), since_ms) {
                guild_spend = guild_spend.saturating_add(used);
            }
        }
    }
    guild_spend
}

/// Compose the autonomous-dispatch budget gate for one Brief: the per-Operative
/// Allowance hard-stop ([`allowance_admits`], authoritative and unchanged)
/// followed by the **additive** Guild-budget hard-stop ([`guild_allowance_admits`],
/// relix-company-model §6.6). The Guild gate runs only when the per-Operative
/// gate allows, so it can never weaken per-Operative enforcement.
///
/// **Tenant isolation:** the Guild spend is the SUM of the Brief's OWN Guild's
/// active Operatives' `cost_since` over the canonical [`allowance_window`]
/// (current UTC calendar month), resolved via
/// `task_tenant` → `list_active_for_tenant`, never a cross-tenant
/// `cost_since(None, …)` — so another Guild's spend can never trip this Guild's
/// cap. This mirrors the Action Center's `company_spend_item` exactly.
///
/// `now_ms` is wall-clock unix-ms (the caller passes it so the function stays
/// deterministic in tests). When `spine`/`metrics` is `None`, the Guild gate is
/// inert (the per-Operative gate is still applied). Used ONLY on the autonomous
/// path — a manual `brief.run` / `prime.start` never calls this (the Board is
/// sovereign).
pub fn dispatch_budget_admits(
    card: &brief::BriefCard,
    task_store: &TaskStore,
    agent_store: &crate::nodes::coordinator::agent::store::AgentStore,
    spine: Option<&crate::nodes::coordinator::spine::SpineStore>,
    metrics: Option<&crate::metrics::MetricsQuery>,
    now_ms: i64,
) -> BudgetAdmission {
    let Some(assignee) = card.assignee_agent_id.as_deref() else {
        return BudgetAdmission::Allow;
    };
    // Canonical Allowance window: the current UTC calendar month, inclusive of
    // its first instant (relix-company-model §6/§6.6). The Action Center reads
    // the SAME window via `MetricsSpendSource::current_month`.
    let since_ms = allowance_window(now_ms).start_ms;

    // (1) Per-Operative Allowance hard-stop — authoritative, never weakened.
    if let Some(agent) = agent_store.get_agent(assignee).ok().flatten() {
        let cap = agent.monthly_allowance_cents;
        if cap.is_some() {
            let spend = metrics
                .and_then(|q| q.cost_since(Some(assignee), since_ms).ok())
                .unwrap_or(0);
            if let BudgetAdmission::Refuse {
                reason,
                event,
                status,
            } = allowance_admits(cap, spend)
            {
                return BudgetAdmission::Refuse {
                    reason: format!(
                        "budget_refused: agent_id={assignee} allowance={}c used={spend}u reason={reason}",
                        cap.unwrap_or(0)
                    ),
                    event,
                    status,
                };
            }
        }
    }

    // (2) Guild-level budget hard-stop — additive, tenant-scoped.
    let (Some(spine), Some(metrics)) = (spine, metrics) else {
        return BudgetAdmission::Allow;
    };
    // The Brief's OWN Guild — so the spend sum below stays tenant-isolated.
    let Ok(Some(tenant)) = task_store.task_tenant(&card.task_id) else {
        return BudgetAdmission::Allow;
    };
    let budget = spine
        .get_guild(&tenant)
        .ok()
        .flatten()
        .and_then(|g| g.monthly_allowance_cents)
        .filter(|b| *b > 0);
    let Some(budget) = budget else {
        return BudgetAdmission::Allow;
    };
    // Canonical Guild month-to-date spend — the exact figure the `guild.spend`
    // operator route reports (one shared helper, one window).
    let guild_spend = guild_spend_micros(agent_store, metrics, &tenant, since_ms);
    match guild_allowance_admits(Some(budget), guild_spend) {
        BudgetAdmission::Allow => BudgetAdmission::Allow,
        BudgetAdmission::Refuse {
            reason,
            event,
            status,
        } => BudgetAdmission::Refuse {
            reason: format!(
                "guild_budget_refused: tenant={tenant} brief={} assignee={assignee} budget={budget}c guild_used={guild_spend}u reason={reason}",
                card.task_id
            ),
            event,
            status,
        },
    }
}

/// Run one full dispatch tick: claim the ready Briefs, run each on
/// its Rig, advance the board, and release the Claim.
///
/// For each claimed Brief:
///   - resolve its Rig via `resolve_rig` (the assignee's chosen
///     backend, or the Guild default — the caller owns that lookup
///     so this stays decoupled from the agent store);
///   - if it has a Rig: move `todo → in_progress` (work has
///     started), run the Rig with the prompt from `build_prompt`,
///     then advance by outcome — `Done` → `in_review`, an
///     unrecoverable `Failed` (`retryable: false`) → `blocked` (so
///     it isn't re-dispatched forever), a `Continue` stays
///     `in_progress` and chronicles its note for the next Shift, a
///     retryable failure stays `in_progress` for the next tick;
///   - if no Rig resolves: record a `Failed` outcome and leave the
///     board untouched (nothing ran — it re-appears next tick / the
///     Desk surfaces it);
///   - always release the Claim afterwards, so a continuation or
///     the next tick can pick the Brief up.
///
/// The board transitions are always valid by construction (the
/// ready set is `todo`/`in_progress`), so they propagate real DB
/// errors but never an illegal-transition error.
pub fn dispatch_batch<R, P>(
    store: &TaskStore,
    batch: usize,
    lease_secs: i64,
    bridge_tokens: Option<&BridgeTokenStore>,
    resolve_rig: R,
    build_prompt: P,
) -> Result<Vec<DispatchRecord>, CoordinatorError>
where
    R: Fn(&brief::BriefCard) -> Option<Arc<dyn Rig>>,
    P: Fn(&brief::BriefCard) -> String,
{
    dispatch_batch_with_policy(
        store,
        batch,
        lease_secs,
        bridge_tokens,
        |_| true,
        |_| 20,
        // No budget gate for the simple wrapper (tests / old callers).
        |_| BudgetAdmission::Allow,
        resolve_rig,
        build_prompt,
        // No per-run model hints for the simple wrapper — the backward-
        // compatible default (the assignee's Rig runs on its own default model).
        |_| RunModelPrefs::default(),
    )
}

/// Policy-aware dispatch tick used by the live controller. The
/// default [`dispatch_batch`] keeps tests and old callers simple;
/// this variant lets production wiring enforce per-agent runtime
/// Keys before queueing timer wakes and before claiming queued runs.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_batch_with_policy<R, P, A, C, B, M>(
    store: &TaskStore,
    batch: usize,
    lease_secs: i64,
    bridge_tokens: Option<&BridgeTokenStore>,
    allow_timer_wakeup: A,
    max_running_for_agent: C,
    admit_budget: B,
    resolve_rig: R,
    build_prompt: P,
    resolve_model_prefs: M,
) -> Result<Vec<DispatchRecord>, CoordinatorError>
where
    R: Fn(&brief::BriefCard) -> Option<Arc<dyn Rig>>,
    P: Fn(&brief::BriefCard) -> String,
    A: Fn(&brief::BriefCard) -> bool,
    C: FnMut(&str) -> i64,
    B: Fn(&brief::BriefCard) -> BudgetAdmission,
    M: Fn(&brief::BriefCard) -> RunModelPrefs,
{
    // Autonomous stale-claim adoption (execution-and-issue-design §1.4
    // "stale-run adoption" / §7.1 LOCKED two-pointer Claim): BEFORE selecting
    // ready Briefs, reclaim any would-be-ready Brief that is blocked ONLY by a
    // LIVE Claim dangling on TERMINAL run evidence — the SAME adoption the
    // manual/Prime start performs via `reclaim_terminal_claim`, sharing that one
    // tested helper. Without this, a live-but-dead Claim is excluded from
    // `list_ready_briefs`, so the heartbeat would wait for the age-based lease
    // sweep before re-dispatching. Safe + idempotent + tenant-safe: each reclaim
    // releases only on terminal evidence matching the Claim's own pointer (never a
    // live run, never a Claim a newer run re-acquired, never another Guild's
    // Claim), chronicles `brief.claim_reclaimed` once, and promotes the oldest
    // deferred wake — so the normal queue/claim path below picks the Brief up this
    // same tick without duplicating wakeups or runs.
    let _ = store.reclaim_terminal_claims_ready(batch)?;
    let ready = store.list_ready_briefs(batch)?;
    for card in &ready {
        let Some(assignee) = card.assignee_agent_id.as_deref() else {
            continue;
        };
        if allow_timer_wakeup(card) {
            let _ =
                store.request_brief_wakeup(&card.task_id, assignee, "timer", "heartbeat", None)?;
        }
    }
    let claimed = store.claim_queued_wakeups_with_caps(batch, lease_secs, max_running_for_agent)?;
    let mut records = Vec::with_capacity(claimed.len());
    for claimed_wake in claimed {
        let card = claimed_wake.card;
        let wakeup_id = claimed_wake.wakeup.wakeup_id;
        // The durable run id the claim step stamped on this Brief's Claim
        // (`execution_run_id`). The committed run below records `brief_runs` under
        // THIS SAME id — never a freshly minted one — so a heartbeat-origin Claim
        // left dangling on a TERMINAL run is adopted by terminal-evidence reclaim
        // (`reclaim_terminal_claim`) exactly like a manual run, closing the old
        // `shift_?`-pointer / `run_?`-ledger asymmetry (execution-and-issue §1.4/§7.1).
        let run_id = claimed_wake.run_id;
        // PHASE 4 (Allowance hard-stop, relix-company-model §3.6/§5.2D):
        // before running the Brief, check the assigned Operative is
        // within budget. If over budget / hard-stopped, do NOT run it
        // and do NOT silently skip — park it in `blocked` (visible to
        // the operator), chronicle WHY, finish the wakeup, and release
        // the Claim so the lease is not leaked.
        if let BudgetAdmission::Refuse {
            reason,
            event,
            status,
        } = admit_budget(&card)
        {
            // `todo -> blocked` is illegal; mirror the dispatch path's
            // `todo -> in_progress -> blocked` so the park is valid.
            if card.board_status == "todo" {
                store.set_board_status(&card.task_id, "in_progress")?;
            }
            store.set_board_status(&card.task_id, "blocked")?;
            // `event` distinguishes the per-Operative Allowance stop
            // (`brief.budget_refused`) from the Guild-budget stop
            // (`guild.budget_refused`) so the Chronicle / Action Center reads
            // honestly which ceiling refused the autonomous run.
            let _ = store.append_event(&card.task_id, event, &reason);
            // Durable refused Shift so latest_run / run history explain WHY the
            // autonomous run didn't happen (no Rig resolved yet → empty).
            let _ = store.record_refused_run(
                &card.task_id,
                card.assignee_agent_id.as_deref().unwrap_or(""),
                "",
                status,
                &reason,
                "heartbeat",
            );
            let _ = store.finish_wakeup(&wakeup_id, "failed", Some(&reason));
            if let Some(assignee) = card.assignee_agent_id.as_deref() {
                store.release_claim(&card.task_id, assignee)?;
            }
            records.push(DispatchRecord {
                brief_id: card.task_id.clone(),
                rig: String::new(),
                outcome: RigOutcome::Failed {
                    reason,
                    retryable: false,
                },
            });
            continue;
        }
        let assignee = card.assignee_agent_id.clone().unwrap_or_default();
        // Resolve the assignee's adapter. No Rig → a clear refusal (NO run
        // row, board untouched); the Desk surfaces it next tick.
        let Some(rig) = resolve_rig(&card) else {
            let reason = "no Rig configured and no Guild default".to_string();
            let _ = store.finish_wakeup(&wakeup_id, "failed", Some(&reason));
            let _ = store.append_event(&card.task_id, "brief.dispatch_failed", &reason);
            let _ = store.record_refused_run(
                &card.task_id,
                &assignee,
                "",
                "no_adapter",
                &reason,
                "heartbeat",
            );
            if !assignee.is_empty() {
                store.release_claim(&card.task_id, &assignee)?;
            }
            records.push(DispatchRecord {
                brief_id: card.task_id.clone(),
                rig: String::new(),
                outcome: RigOutcome::Failed {
                    reason,
                    retryable: false,
                },
            });
            continue;
        };
        // Same readiness gate as a manual run: NEVER spawn an adapter that
        // isn't available. A refusal records NO run row (matches manual
        // pre-flight semantics) — it's a refusal, not an execution failure.
        let probe = rig.probe();
        if probe.status != "available" {
            let reason = format!("adapter `{}` unavailable: {}", rig.name(), probe.detail);
            let _ = store.finish_wakeup(&wakeup_id, "failed", Some(&reason));
            let _ = store.append_event(&card.task_id, "brief.dispatch_failed", &reason);
            let _ = store.record_refused_run(
                &card.task_id,
                &assignee,
                rig.name(),
                "adapter_unavailable",
                &reason,
                "heartbeat",
            );
            if !assignee.is_empty() {
                store.release_claim(&card.task_id, &assignee)?;
            }
            records.push(DispatchRecord {
                brief_id: card.task_id.clone(),
                rig: rig.name().to_string(),
                outcome: RigOutcome::Failed {
                    reason,
                    retryable: false,
                },
            });
            continue;
        }
        // Commit + execute through the SHARED pipeline — the SAME path a
        // manual dashboard run takes — so an autonomous run produces the
        // identical durable output: ledger row, transcript, artifacts,
        // review state, apply eligibility. The only difference is the
        // `heartbeat` trigger stamped on the run record.
        let rig_name = rig.name().to_string();
        let prompt = build_prompt(&card);
        // The assigned Operative's stored model/effort hints for this run —
        // the caller owns the agent lookup (mirrors `resolve_rig`), so this
        // stays decoupled from the agent store.
        let prefs = resolve_model_prefs(&card);
        match prepare_claimed_run(
            store,
            bridge_tokens,
            lease_secs,
            &card,
            &assignee,
            rig,
            &run_id,
            prompt,
            RunTrigger::Heartbeat,
            prefs,
        )? {
            // Workspace prep refused → no run row opened, board untouched.
            Err(refusal) => {
                let _ = store.finish_wakeup(&wakeup_id, "failed", Some(&refusal.summary));
                let _ =
                    store.append_event(&card.task_id, "brief.dispatch_failed", &refusal.summary);
                // `refusal.status` is `workspace_error` / `workspace_context_error`.
                let _ = store.record_refused_run(
                    &card.task_id,
                    &assignee,
                    &rig_name,
                    &refusal.status,
                    &refusal.summary,
                    "heartbeat",
                );
                if !assignee.is_empty() {
                    store.release_claim(&card.task_id, &assignee)?;
                }
                records.push(DispatchRecord {
                    brief_id: card.task_id.clone(),
                    rig: rig_name,
                    outcome: RigOutcome::Failed {
                        reason: refusal.summary,
                        retryable: false,
                    },
                });
            }
            // Committed → run it. `execute_ready_inner` advances the board,
            // chronicles, scans artifacts, sets review state, closes the run
            // row, AND releases the Claim. We only finish the wakeup here.
            Ok(ready) => {
                let (report, outcome) = execute_ready_inner(store, bridge_tokens, ready);
                let wakeup_status = match report.status.as_str() {
                    "done" => "completed",
                    "continued" => "continued",
                    "cancelled" => "cancelled",
                    _ => "failed",
                };
                let _ = store.finish_wakeup(&wakeup_id, wakeup_status, Some(&report.summary));
                // Rebuild the dispatch outcome from the run result, keeping
                // the raw `retryable` distinction for a genuine failure.
                let record_outcome = match report.status.as_str() {
                    "done" => RigOutcome::Done {
                        summary: report.summary.clone(),
                    },
                    "continued" => RigOutcome::Continue {
                        note: report.summary.clone(),
                    },
                    "cancelled" => RigOutcome::Failed {
                        retryable: false,
                        reason: report.summary.clone(),
                    },
                    _ => outcome,
                };
                records.push(DispatchRecord {
                    brief_id: card.task_id.clone(),
                    rig: rig_name,
                    outcome: record_outcome,
                });
            }
        }
    }
    Ok(records)
}

/// Structured result of a manual, synchronous **run** of one Brief —
/// the dashboard "Start / Run" path (`brief.run`). Unlike the timer
/// loop, this runs immediately and reports a clear outcome, including
/// the adapter-unavailable states (so the UI never fakes a run).
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunReport {
    pub brief_id: String,
    /// `done` / `failed` / `continued` — real run outcomes; or
    /// `not_found` / `unassigned` / `no_adapter` / `adapter_unavailable`
    /// / `already_running` — pre-run refusals (no command was spawned).
    pub status: String,
    /// The adapter (Rig) that ran it, empty when none resolved.
    pub rig: String,
    /// Result summary (Done) or reason (Failed / refusal). Already
    /// secret-redacted by the Rig before it reaches here.
    pub summary: String,
    /// Install hint when the adapter is missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_hint: Option<String>,
    /// The durable run record id (`brief_runs.run_id`) once a run is
    /// committed — `None` for a pre-flight refusal that never ran. The
    /// dashboard polls `/v1/runs` for this id to watch the run finish.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The scoped per-run workspace the Rig executes in — so the operator
    /// sees WHERE the run happened. `None` for a refusal / `inherit` mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Workspace context mode (`empty` / `copy_repo`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_context: Option<String>,
    /// Files copied into the workspace (`copy_repo`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_files: Option<i64>,
    /// Bytes copied into the workspace (`copy_repo`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_bytes: Option<i64>,
}

impl RunReport {
    fn refuse(brief_id: &str, status: &str, summary: impl Into<String>) -> Self {
        Self {
            brief_id: brief_id.to_string(),
            status: status.to_string(),
            rig: String::new(),
            summary: summary.into(),
            install_hint: None,
            run_id: None,
            workspace: None,
            workspace_context: None,
            workspace_files: None,
            workspace_bytes: None,
        }
    }
}

/// A Brief that has cleared pre-flight (assigned, adapter available,
/// Claim won, run record opened) and is ready to execute on its Rig.
/// Carries everything [`execute_ready`] needs so the actual `rig.run`
/// can be moved onto a blocking background thread (async dispatch)
/// without re-touching the store under the async runtime.
pub struct ReadyRun {
    pub brief_id: String,
    pub assignee: String,
    pub run_id: String,
    pub rig_name: String,
    /// The scoped per-run workspace the Rig will execute in (`None` in
    /// `inherit` mode — legacy coordinator-CWD execution).
    pub workspace: Option<String>,
    /// Workspace context mode (`empty` / `copy_repo`) + copy stats, carried
    /// through to the terminal RunReport.
    pub workspace_context: Option<String>,
    pub workspace_files: Option<i64>,
    pub workspace_bytes: Option<i64>,
    /// Snapshot of the workspace files BEFORE the run, used after to detect
    /// what the agent changed. Empty in `inherit` mode (no scoped dir).
    baseline: WorkspaceManifest,
    rig: std::sync::Arc<dyn Rig>,
    req: RigRunRequest,
    token: String,
}

/// Per-run workspace mode. Default `scoped` (a dedicated dir per run);
/// `inherit` opts OUT (legacy coordinator-CWD execution) and must be
/// explicitly set, so a Brief never runs repo-wide by accident.
fn workspace_mode_is_inherit() -> bool {
    std::env::var("RELIX_RUN_WORKSPACE_MODE")
        .map(|v| v.trim().eq_ignore_ascii_case("inherit"))
        .unwrap_or(false)
}

/// A run id is safe to use as a single workspace path segment iff it is
/// our generated `run_<uuid>` shape: non-empty, bounded, alphanumeric +
/// `_`/`-` only (no separators, no `.`/`..`). The traversal defense — the
/// path is derived ONLY from this, never from Brief content.
pub fn run_id_is_safe(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id.len() <= 80
        && run_id != "."
        && run_id != ".."
        && run_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Make a path absolute WITHOUT canonicalizing (no `\\?\` prefix, no
/// symlink resolution) — clean enough to show an operator and to use as a
/// child `current_dir`.
fn absolute_clean(p: &std::path::Path) -> std::path::PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

/// How much project context a run workspace receives. Default `Empty`
/// (the safest: only `BRIEF.md`). `CopyRepo` copies a capped, filtered
/// snapshot of the project so real coding work can happen WITHOUT going
/// back to dangerous repo-CWD execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WorkspaceContext {
    #[default]
    Empty,
    CopyRepo,
}

impl WorkspaceContext {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkspaceContext::Empty => "empty",
            WorkspaceContext::CopyRepo => "copy_repo",
        }
    }
}

/// Resolved run-workspace context configuration. Read from env at store
/// open; injectable in tests. The Brief prompt NEVER influences any of
/// these — the project root + caps are operator config only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceConfig {
    pub context: WorkspaceContext,
    /// The directory `copy_repo` snapshots from. Defaults to the
    /// coordinator's CWD; overridable via `RELIX_RUN_PROJECT_ROOT`.
    pub project_root: std::path::PathBuf,
    /// Hard cap on total copied bytes (`copy_repo`).
    pub max_bytes: u64,
    /// Hard cap on total copied file count (`copy_repo`).
    pub max_files: usize,
}

/// Conservative defaults — small enough that an accidental `copy_repo` of
/// a huge tree fails fast instead of bloating disk (we've had a 150GB
/// local-bloat incident; `copy_repo` is explicit, capped, observable).
pub const DEFAULT_WORKSPACE_MAX_BYTES: u64 = 32 * 1024 * 1024; // 32 MiB
pub const DEFAULT_WORKSPACE_MAX_FILES: usize = 2_000;

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            context: WorkspaceContext::Empty,
            project_root: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            max_bytes: DEFAULT_WORKSPACE_MAX_BYTES,
            max_files: DEFAULT_WORKSPACE_MAX_FILES,
        }
    }
}

/// Resolve the run-workspace context config from env. Unknown / unset
/// context → `Empty` (safe default).
pub fn resolve_workspace_config() -> WorkspaceConfig {
    let context = match std::env::var("RELIX_RUN_WORKSPACE_CONTEXT")
        .ok()
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("copy_repo") | Some("copy-repo") => WorkspaceContext::CopyRepo,
        _ => WorkspaceContext::Empty,
    };
    let project_root = std::env::var_os("RELIX_RUN_PROJECT_ROOT")
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        });
    let max_bytes = std::env::var("RELIX_RUN_WORKSPACE_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|&n: &u64| n > 0)
        .unwrap_or(DEFAULT_WORKSPACE_MAX_BYTES);
    let max_files = std::env::var("RELIX_RUN_WORKSPACE_MAX_FILES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|&n: &usize| n > 0)
        .unwrap_or(DEFAULT_WORKSPACE_MAX_FILES);
    WorkspaceConfig {
        context,
        project_root,
        max_bytes,
        max_files,
    }
}

/// Directory names NEVER copied by `copy_repo` (any depth): VCS, build
/// caches, dependency trees, generated data, and the run-workspace tree
/// itself (so a copy can't recurse into its own output). Case-insensitive.
const EXCLUDED_DIR_NAMES: &[&str] = &[
    ".git",
    "target",
    "target-audit",
    "node_modules",
    "dev-data",
    "workspaces",
    "worktrees", // covers .claude/worktrees
    "dev-keys",
    "references",
    ".venv",
    "venv",
    "__pycache__",
    ".cargo",
    "dist",
];

/// A file is excluded from `copy_repo` when its name is dotenv or carries
/// obvious secret/key material — never copy credentials into a sandbox.
fn is_excluded_file(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == ".env"
        || n.starts_with(".env.")
        || n.ends_with(".key")
        || n.ends_with(".pem")
        || n.ends_with(".p12")
        || n.ends_with(".pfx")
        || n.ends_with(".pub")
        || n.ends_with(".aic")
        || n.ends_with(".keystore")
        || n.starts_with("id_rsa")
        || n.starts_with("id_ed25519")
        || n.starts_with("id_ecdsa")
        || n == "bridge-token"
        || n == "dashboard-admin.json"
        || n.contains("secret")
        || n.contains("credential")
        || n.contains("password")
}

fn is_excluded_dir(name: &str) -> bool {
    EXCLUDED_DIR_NAMES
        .iter()
        .any(|d| d.eq_ignore_ascii_case(name))
}

/// Best-effort `.gitignore` "respect where practical": read the project
/// root's top-level `.gitignore` and collect the bare names it ignores
/// (stripping leading/trailing `/`, dropping globs / negations / nested
/// paths). The hardcoded [`EXCLUDED_DIR_NAMES`] remain the real safety
/// net; this just honors common project ignores like a tracked
/// `references/` or `coverage/`.
fn gitignore_names(project_root: &std::path::Path) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let Ok(text) = std::fs::read_to_string(project_root.join(".gitignore")) else {
        return set;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let name = line.trim_matches('/');
        // Only honor simple single-segment names without globs.
        if !name.is_empty()
            && !name.contains('/')
            && !name.contains('*')
            && !name.contains('?')
            && !name.contains('[')
        {
            set.insert(name.to_string());
        }
    }
    set
}

/// Validate the configured project root is a safe directory to snapshot.
/// Rejects a missing/non-dir path and a filesystem/drive root (no parent),
/// so `copy_repo` can never walk `/` or `C:\`. The root is operator config,
/// never Brief-derived.
fn validate_project_root(root: &std::path::Path) -> Result<std::path::PathBuf, String> {
    if !root.is_dir() {
        return Err(format!(
            "project root is not a directory: {}",
            root.display()
        ));
    }
    let abs = std::fs::canonicalize(root)
        .map_err(|e| format!("cannot resolve project root {}: {e}", root.display()))?;
    if abs.parent().is_none() {
        return Err(format!(
            "refusing to snapshot a filesystem root: {}",
            abs.display()
        ));
    }
    Ok(abs)
}

/// Copy a capped, filtered snapshot of `src_root` into `dst`. Skips
/// symlinks (no escaping the root, no loops), excluded dirs/files, and
/// `.gitignore` names. Enforces file-count + byte caps, aborting with a
/// clear error the moment a cap is exceeded. Returns `(files, bytes)`.
fn copy_repo_into(
    src_root: &std::path::Path,
    dst: &std::path::Path,
    cfg: &WorkspaceConfig,
) -> Result<(usize, u64), String> {
    let gitignore = gitignore_names(src_root);
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![src_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("entry: {e}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let ft = entry
                .file_type()
                .map_err(|e| format!("file type {name}: {e}"))?;
            // Symlinks are never followed — they could escape the root or
            // form cycles.
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if is_excluded_dir(&name) || gitignore.contains(&name) {
                    continue;
                }
                stack.push(entry.path());
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            if is_excluded_file(&name) || gitignore.contains(&name) {
                continue;
            }
            let sz = entry.metadata().map(|m| m.len()).unwrap_or(0);
            files += 1;
            bytes = bytes.saturating_add(sz);
            if files > cfg.max_files {
                return Err(format!(
                    "file-count cap exceeded ({} > {} files) — raise RELIX_RUN_WORKSPACE_MAX_FILES or narrow the project root",
                    files, cfg.max_files
                ));
            }
            if bytes > cfg.max_bytes {
                return Err(format!(
                    "size cap exceeded ({} > {} bytes) — raise RELIX_RUN_WORKSPACE_MAX_BYTES or narrow the project root",
                    bytes, cfg.max_bytes
                ));
            }
            let rel = entry
                .path()
                .strip_prefix(src_root)
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|_| std::path::PathBuf::from(&name));
            let target = dst.join(&rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("copy {}: {e}", rel.display()))?;
        }
    }
    Ok((files, bytes))
}

/// A run workspace that has been prepared (created + context applied).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedWorkspace {
    pub path: std::path::PathBuf,
    pub context: WorkspaceContext,
    pub copied_files: usize,
    pub copied_bytes: u64,
}

/// Why workspace prep failed — distinguishes the basic-creation refusal
/// (`workspace_error`) from a context-copy / cap refusal
/// (`workspace_context_error`).
#[derive(Debug)]
pub enum WorkspacePrepError {
    Create(String),
    Context(String),
}

impl WorkspacePrepError {
    pub fn status(&self) -> &'static str {
        match self {
            WorkspacePrepError::Create(_) => "workspace_error",
            WorkspacePrepError::Context(_) => "workspace_context_error",
        }
    }
    pub fn message(&self) -> &str {
        match self {
            WorkspacePrepError::Create(m) | WorkspacePrepError::Context(m) => m,
        }
    }
}

/// Create the scoped per-run workspace `<root>/<run_id>`, drop a small
/// trusted `BRIEF.md`, and apply the configured context (`empty` →
/// nothing; `copy_repo` → a capped, filtered project snapshot). Returns
/// the prepared workspace (path + mode + copy stats). The path is derived
/// ONLY from the validated `run_id` + the configured root — NEVER from
/// Brief content — so a prompt can't choose where work lands, and
/// traversal is impossible. On a context-copy failure the partial
/// workspace is removed so no half-copied tree lingers.
pub fn prepare_run_workspace(
    root: &std::path::Path,
    run_id: &str,
    brief_id: &str,
    title: &str,
    context: &str,
    cfg: &WorkspaceConfig,
) -> Result<PreparedWorkspace, WorkspacePrepError> {
    if !run_id_is_safe(run_id) {
        return Err(WorkspacePrepError::Create(format!(
            "unsafe run id for workspace: {run_id:?}"
        )));
    }
    std::fs::create_dir_all(root)
        .map_err(|e| WorkspacePrepError::Create(format!("create workspace root: {e}")))?;
    let root = absolute_clean(root);
    let ws = root.join(run_id);
    // Belt-and-suspenders traversal guard: with a validated single-segment
    // run_id the workspace is always a DIRECT child of root.
    if ws.parent() != Some(root.as_path()) {
        return Err(WorkspacePrepError::Create(format!(
            "workspace path escapes its root: {}",
            ws.display()
        )));
    }
    std::fs::create_dir_all(&ws)
        .map_err(|e| WorkspacePrepError::Create(format!("create workspace: {e}")))?;
    // A small, trusted instruction file — no secrets, no prompt-chosen
    // path. Best-effort: a write failure does not fail the run.
    let brief_md = format!(
        "# Relix Brief workspace\n\n\
         - brief_id: {brief_id}\n\
         - run_id: {run_id}\n\
         - title: {title}\n\
         - context: {context}\n\
         - workspace_context: {ctx_mode}\n\n\
         This folder is the scoped sandbox for one Brief run. Keep all work \
         for this Brief inside it.\n",
        ctx_mode = cfg.context.as_str(),
    );
    let _ = std::fs::write(ws.join("BRIEF.md"), brief_md);

    let (copied_files, copied_bytes) = match cfg.context {
        WorkspaceContext::Empty => (0, 0),
        WorkspaceContext::CopyRepo => {
            let src = validate_project_root(&cfg.project_root).map_err(|e| {
                let _ = std::fs::remove_dir_all(&ws);
                WorkspacePrepError::Context(format!("invalid project root: {e}"))
            })?;
            match copy_repo_into(&src, &ws, cfg) {
                Ok(stats) => stats,
                Err(e) => {
                    // Remove the partial copy so no half-snapshot lingers.
                    let _ = std::fs::remove_dir_all(&ws);
                    return Err(WorkspacePrepError::Context(e));
                }
            }
        }
    };
    Ok(PreparedWorkspace {
        path: ws,
        context: cfg.context,
        copied_files,
        copied_bytes,
    })
}

// ── Run artifacts: detect what the agent changed in the workspace ──

/// Don't hash files larger than this — `size` alone decides "modified"
/// for big files (avoids reading huge blobs just to detect a change).
const ARTIFACT_HASH_MAX_BYTES: u64 = 1024 * 1024; // 1 MiB
/// Hard cap on files walked during an artifact scan (safety net on top of
/// the workspace copy caps).
const ARTIFACT_SCAN_MAX_FILES: usize = 8000;

/// A lightweight signature of one workspace file for change detection.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileSig {
    size: u64,
    /// Content hash for files ≤ [`ARTIFACT_HASH_MAX_BYTES`]; `None` for
    /// larger (then `size` alone decides "modified").
    hash: Option<u64>,
    is_text: bool,
}

/// A manifest of a workspace's files at one instant, used as the before /
/// after baseline for change detection.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceManifest {
    files: std::collections::HashMap<String, FileSig>,
    /// True when the walk hit [`ARTIFACT_SCAN_MAX_FILES`] (partial scan).
    truncated: bool,
}

/// Heuristic: treat the first chunk of `bytes` as text iff it has no NUL
/// byte (the cheap, reliable binary tell). Empty = text.
fn looks_text(bytes: &[u8]) -> bool {
    !bytes.iter().take(8192).any(|&b| b == 0)
}

/// Signature of one file: size, a deterministic content hash for small
/// files, and a text/binary flag. `DefaultHasher` is fixed-key so the
/// hash is comparable across the before/after scans (same process).
fn file_sig(path: &std::path::Path, size: u64) -> FileSig {
    use std::hash::{Hash, Hasher};
    let mut head = Vec::new();
    let is_text;
    let hash = if size <= ARTIFACT_HASH_MAX_BYTES {
        match std::fs::read(path) {
            Ok(bytes) => {
                is_text = looks_text(&bytes);
                let mut h = std::collections::hash_map::DefaultHasher::new();
                bytes.hash(&mut h);
                Some(h.finish())
            }
            Err(_) => {
                is_text = false;
                None
            }
        }
    } else {
        // Big file: sniff only the head for the text flag; don't hash.
        if let Ok(mut f) = std::fs::File::open(path) {
            use std::io::Read;
            let mut buf = [0u8; 8192];
            if let Ok(n) = f.read(&mut buf) {
                head.extend_from_slice(&buf[..n]);
            }
        }
        is_text = looks_text(&head);
        None
    };
    FileSig {
        size,
        hash,
        is_text,
    }
}

/// Walk a run workspace and build its [`WorkspaceManifest`], reusing the
/// copy-filter exclusions (no `.git`/`target`/`node_modules`/`dev-data`/
/// secrets/…), skipping symlinks, and capping the file count. Paths are
/// recorded relative to `root` with `/` separators.
pub fn scan_workspace_manifest(root: &std::path::Path) -> WorkspaceManifest {
    let mut m = WorkspaceManifest::default();
    if !root.is_dir() {
        return m;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                if !is_excluded_dir(&name) {
                    stack.push(entry.path());
                }
                continue;
            }
            if !ft.is_file() || is_excluded_file(&name) {
                continue;
            }
            if m.files.len() >= ARTIFACT_SCAN_MAX_FILES {
                m.truncated = true;
                return m;
            }
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| name.clone());
            let size = entry.metadata().map(|md| md.len()).unwrap_or(0);
            m.files.insert(rel, file_sig(&path, size));
        }
    }
    m
}

/// One detected change between a before / after manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactChange {
    pub rel_path: String,
    /// `created` / `modified` / `deleted`.
    pub kind: &'static str,
    pub size: u64,
    pub hash: Option<String>,
    /// BEFORE-run content hash (the baseline) for `modified` / `deleted` —
    /// `None` for `created` and for files too large to hash. Persisted on
    /// the artifact so safe-apply can confirm the project-root file still
    /// matches what the run started from.
    pub baseline_hash: Option<String>,
    pub is_text: bool,
}

/// Diff two manifests into the agent's changes (created / modified /
/// deleted), sorted by path. `unchanged` files are intentionally NOT
/// returned — only what the run actually touched (so `BRIEF.md`, copied
/// context, etc. don't show up unless the agent edited them).
pub fn diff_manifests(
    before: &WorkspaceManifest,
    after: &WorkspaceManifest,
) -> Vec<ArtifactChange> {
    let mut out = Vec::new();
    for (rel, sig) in &after.files {
        match before.files.get(rel) {
            None => out.push(ArtifactChange {
                rel_path: rel.clone(),
                kind: "created",
                size: sig.size,
                hash: sig.hash.map(|h| format!("{h:016x}")),
                baseline_hash: None,
                is_text: sig.is_text,
            }),
            Some(prev) if prev != sig => out.push(ArtifactChange {
                rel_path: rel.clone(),
                kind: "modified",
                size: sig.size,
                hash: sig.hash.map(|h| format!("{h:016x}")),
                baseline_hash: prev.hash.map(|h| format!("{h:016x}")),
                is_text: sig.is_text,
            }),
            Some(_) => {} // unchanged — not an artifact
        }
    }
    for (rel, sig) in &before.files {
        if !after.files.contains_key(rel) {
            out.push(ArtifactChange {
                rel_path: rel.clone(),
                kind: "deleted",
                size: 0,
                hash: None,
                baseline_hash: sig.hash.map(|h| format!("{h:016x}")),
                is_text: sig.is_text,
            });
        }
    }
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    out
}

/// Result of reading a small-text artifact preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewOutcome {
    /// A safe, secret-redacted text preview (possibly truncated).
    Text { content: String, truncated: bool },
    /// Binary / non-text — refused (no preview).
    Binary,
    /// The file no longer exists (e.g. a deleted artifact).
    Missing,
    /// The resolved path escaped the workspace — refused.
    Unsafe,
}

/// Read a length-bounded, secret-redacted text preview of an artifact at
/// `<workspace>/<rel_path>`. The path is built ONLY from the stored
/// (server-side) artifact row, and is verified to stay UNDER the
/// workspace (no traversal); binaries and missing files are refused.
pub fn read_artifact_preview(
    workspace: &str,
    rel_path: &str,
    is_text: bool,
    max_bytes: usize,
) -> PreviewOutcome {
    if !is_text {
        return PreviewOutcome::Binary;
    }
    let path = std::path::Path::new(workspace).join(rel_path);
    let (Ok(ws_canon), Ok(file_canon)) = (
        std::fs::canonicalize(workspace),
        std::fs::canonicalize(&path),
    ) else {
        return PreviewOutcome::Missing;
    };
    if !file_canon.starts_with(&ws_canon) {
        return PreviewOutcome::Unsafe;
    }
    let bytes = match std::fs::read(&file_canon) {
        Ok(b) => b,
        Err(_) => return PreviewOutcome::Missing,
    };
    // Re-check binary on the actual bytes (the row flag could be stale).
    if !looks_text(&bytes) {
        return PreviewOutcome::Binary;
    }
    let truncated = bytes.len() > max_bytes;
    let slice = &bytes[..bytes.len().min(max_bytes)];
    let raw = String::from_utf8_lossy(slice).into_owned();
    PreviewOutcome::Text {
        content: crate::rig::redact_secrets(&raw, ""),
        truncated,
    }
}

/// Outcome of building a bounded unified diff for one changed run file.
pub enum DiffOutcome {
    /// A bounded unified diff. `baseline` names where the "before" side came
    /// from: `project_root` (the live file still matches the run's recorded
    /// baseline hash) or `empty` (a created file — diffed against nothing).
    Unified {
        diff: String,
        truncated: bool,
        baseline: &'static str,
    },
    /// No honest diff is possible — the caller should fall back to the file
    /// PREVIEW. `reason` explains why (binary / baseline diverged or missing /
    /// unsafe path / unreadable).
    Unavailable { reason: String },
}

/// Read a file as bounded text: refuses a binary file, caps at `max_bytes`,
/// and reports truncation. Path safety is the caller's responsibility (this
/// is only ever called on a path already validated under the workspace or the
/// project root).
fn read_text_bounded(path: &std::path::Path, max_bytes: usize) -> Result<(String, bool), String> {
    let bytes = std::fs::read(path).map_err(|_| "file unreadable".to_string())?;
    if !looks_text(&bytes) {
        return Err("binary or non-text file — no diff".into());
    }
    let truncated = bytes.len() > max_bytes;
    let slice = &bytes[..bytes.len().min(max_bytes)];
    Ok((String::from_utf8_lossy(slice).into_owned(), truncated))
}

/// Build a SAFE, bounded unified diff for ONE changed file of a run.
///
/// The "after" side is the file in the run's scoped workspace (read with the
/// SAME canonicalize-under-workspace guard the preview uses, already
/// secret-redacted). The "before" side is reconstructed from the LIVE
/// project-root file — but ONLY when it still hashes to the run's recorded
/// `baseline_hash`; if the project file moved since the run we return
/// `Unavailable` (honest — we cannot claim what the run changed against a
/// moved baseline). `created` diffs against an empty baseline; `deleted` diffs
/// the baseline against empty. Both sides are byte-bounded BEFORE diffing and
/// the diff is secret-redacted, so it can never dump a whole repo or a secret.
pub fn read_artifact_diff(
    workspace: &str,
    project_root: &std::path::Path,
    rel_path: &str,
    kind: &str,
    is_text: bool,
    baseline_hash: Option<&str>,
    max_bytes: usize,
) -> DiffOutcome {
    if !is_text {
        return DiffOutcome::Unavailable {
            reason: "binary or non-text file — no diff".into(),
        };
    }
    // Defensive path validation (mirrors apply): never traverse / escape /
    // touch an excluded (secret/build) path.
    if !apply_rel_path_is_safe(rel_path) || apply_path_excluded(rel_path) {
        return DiffOutcome::Unavailable {
            reason: "path refused".into(),
        };
    }
    // "after" side — the run's workspace output (empty for a deletion).
    let after = if kind == "deleted" {
        String::new()
    } else {
        match read_artifact_preview(workspace, rel_path, is_text, max_bytes) {
            PreviewOutcome::Text { content, .. } => content,
            PreviewOutcome::Binary => {
                return DiffOutcome::Unavailable {
                    reason: "binary or non-text file — no diff".into(),
                };
            }
            PreviewOutcome::Missing => {
                return DiffOutcome::Unavailable {
                    reason: "file no longer exists in the workspace".into(),
                };
            }
            PreviewOutcome::Unsafe => {
                return DiffOutcome::Unavailable {
                    reason: "path refused (outside workspace)".into(),
                };
            }
        }
    };
    // "before" side.
    let (before, baseline_label): (String, &'static str) = match kind {
        "created" => (String::new(), "empty"),
        "modified" | "deleted" => {
            let Some(bh) = baseline_hash else {
                return DiffOutcome::Unavailable {
                    reason: "no baseline recorded for this file — preview the run output instead"
                        .into(),
                };
            };
            let Ok(root_canon) = std::fs::canonicalize(project_root) else {
                return DiffOutcome::Unavailable {
                    reason: "project root unavailable".into(),
                };
            };
            let Ok(target) = resolve_apply_target(&root_canon, rel_path) else {
                return DiffOutcome::Unavailable {
                    reason: "path refused".into(),
                };
            };
            // The live file is the baseline ONLY if it still matches the hash
            // the run captured before it started.
            match hash_file_hex(&target) {
                Some(h) if h == bh => match read_text_bounded(&target, max_bytes) {
                    Ok((s, _)) => (crate::rig::redact_secrets(&s, ""), "project_root"),
                    Err(reason) => return DiffOutcome::Unavailable { reason },
                },
                _ => {
                    return DiffOutcome::Unavailable {
                        reason: "the project file changed since this run — diff unavailable; \
                                 preview the run output instead"
                            .into(),
                    };
                }
            }
        }
        other => {
            return DiffOutcome::Unavailable {
                reason: format!("unknown change kind: {other}"),
            };
        }
    };
    // Diff the bounded, redacted sides. `diffy` emits a unified diff.
    let raw = diffy::create_patch(&before, &after).to_string();
    let truncated = raw.len() > max_bytes;
    let bounded = if truncated {
        String::from_utf8_lossy(&raw.as_bytes()[..max_bytes]).into_owned()
    } else {
        raw
    };
    DiffOutcome::Unified {
        diff: crate::rig::redact_secrets(&bounded, ""),
        truncated,
        baseline: baseline_label,
    }
}

// ── Safe apply: copy an accepted run's changed files into the project ──
//
// (relix-execution-and-issue-design — recovery / result handling.) After an
// operator ACCEPTS a run, its changed files can be applied back into the
// configured project root. The design philosophy is conservative: validate
// every path, compare each target against the run's baseline, and refuse the
// WHOLE apply if ANYTHING is unsafe — better to refuse than overwrite blindly.

/// Largest file safe-apply will hash to verify a target — mirrors
/// [`ARTIFACT_HASH_MAX_BYTES`]. A file above this can't be content-verified,
/// so a `modified`/`deleted` of such a file is refused.
const APPLY_HASH_MAX_BYTES: u64 = ARTIFACT_HASH_MAX_BYTES;

/// One file's plan in a safe apply — what WOULD happen to its project-root
/// copy. Pure preview; building it never mutates the filesystem.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ApplyPlanItem {
    pub rel_path: String,
    /// The run's change: `created` / `modified` / `deleted`.
    pub kind: String,
    /// What apply would do: `create` / `overwrite` / `delete` / `noop`
    /// (target already in the desired state) / `refuse`.
    pub action: &'static str,
    /// True iff safe to apply (a `noop` counts as safe).
    pub can_apply: bool,
    /// True iff the target diverged from the run's baseline (a real
    /// conflict, distinct from a structural refusal like a bad path).
    pub conflict: bool,
    pub reason: String,
    pub source_size: u64,
    pub target_exists: bool,
}

/// The full safe-apply plan for a run. `applicable` is true only when EVERY
/// item is safe; otherwise apply refuses the whole run (no partial apply).
#[derive(Clone, Debug, serde::Serialize)]
pub struct ApplyPlan {
    pub project_root: String,
    pub items: Vec<ApplyPlanItem>,
    pub applicable: bool,
    /// Items that would actually write or delete (excludes noops).
    pub changes: usize,
    /// Items refused because the target diverged from baseline.
    pub conflicts: usize,
    /// Items refused for a structural reason (bad path / excluded / source
    /// missing / unverifiable).
    pub blocked: usize,
    pub note: String,
}

/// Outcome of an apply attempt (plan executed, or refused unchanged).
#[derive(Clone, Debug)]
pub struct ApplyOutcome {
    pub plan: ApplyPlan,
    /// Durable status: `applied` / `conflicted` / `failed`.
    pub status: &'static str,
    pub applied_files: usize,
    pub failed_files: usize,
    pub errors: Vec<String>,
}

/// Is a stored artifact `rel_path` safe to apply? It must be a relative,
/// forward-only path: non-empty, no drive letter, no UNC, no leading
/// separator, no `.`/`..`/empty component. (Artifacts are stored with `/`
/// separators; we re-validate defensively and split on BOTH separators so a
/// `\`-bearing path can't smuggle traversal.)
fn apply_rel_path_is_safe(rel_path: &str) -> bool {
    if rel_path.is_empty() || rel_path.starts_with('/') || rel_path.starts_with('\\') {
        return false;
    }
    let b = rel_path.as_bytes();
    if b.len() >= 2 && b[1] == b':' {
        return false; // drive-letter (`C:...`)
    }
    rel_path
        .split(['/', '\\'])
        .all(|c| !c.is_empty() && c != "." && c != "..")
}

/// Is any directory component an excluded dir, or the final component an
/// excluded file? Safe-apply never writes into `.git`/`target`/… and never
/// applies secret/key files — the same filter the workspace copy uses.
fn apply_path_excluded(rel_path: &str) -> bool {
    let comps: Vec<&str> = rel_path.split(['/', '\\']).collect();
    let n = comps.len();
    comps.iter().enumerate().any(|(i, comp)| {
        if i + 1 == n {
            is_excluded_file(comp)
        } else {
            is_excluded_dir(comp)
        }
    })
}

/// Hash a file with the SAME scheme as [`file_sig`] so a target's hash is
/// comparable to an artifact's stored before/after hash. `None` if missing,
/// unreadable, not a regular file, or larger than the hash cap.
fn hash_file_hex(path: &std::path::Path) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let md = std::fs::symlink_metadata(path).ok()?;
    if !md.is_file() || md.len() > APPLY_HASH_MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    Some(format!("{:016x}", h.finish()))
}

/// Resolve `<root_canon>/<rel_path>` to a target path, refusing anything
/// that escapes the root or traverses a symlink. Every EXISTING component
/// (including the final one, so a symlinked target file is caught) must not
/// be a symlink; the first non-existent component ends the check (it'll be
/// created). `root_canon` must already be canonicalized.
fn resolve_apply_target(
    root_canon: &std::path::Path,
    rel_path: &str,
) -> Result<std::path::PathBuf, String> {
    let mut target = root_canon.to_path_buf();
    for comp in rel_path.split(['/', '\\']) {
        target.push(comp);
    }
    if !target.starts_with(root_canon) {
        return Err("path escapes the project root".into());
    }
    let mut cur = root_canon.to_path_buf();
    for comp in rel_path.split(['/', '\\']) {
        cur.push(comp);
        match std::fs::symlink_metadata(&cur) {
            Ok(md) if md.file_type().is_symlink() => {
                return Err(format!("path crosses a symlink: {comp}"));
            }
            Ok(_) => {}
            Err(_) => break, // doesn't exist yet — will be created
        }
    }
    Ok(target)
}

/// Decide the apply plan for one artifact against the (canonical) project
/// root. PURE — reads the filesystem to compare hashes, never writes.
fn plan_one(root_canon: &std::path::Path, art: &super::RunArtifact) -> ApplyPlanItem {
    let rel = art.rel_path.clone();
    let kind = art.kind.clone();
    let mk = |action: &'static str,
              can_apply: bool,
              conflict: bool,
              reason: String,
              source_size: u64,
              target_exists: bool| ApplyPlanItem {
        rel_path: rel.clone(),
        kind: kind.clone(),
        action,
        can_apply,
        conflict,
        reason,
        source_size,
        target_exists,
    };

    if !apply_rel_path_is_safe(&rel) {
        return mk(
            "refuse",
            false,
            false,
            "unsafe path (absolute / traversal / drive / UNC)".into(),
            0,
            false,
        );
    }
    if apply_path_excluded(&rel) {
        return mk(
            "refuse",
            false,
            false,
            "excluded path (vcs/build/secret) — never applied".into(),
            0,
            false,
        );
    }
    let target = match resolve_apply_target(root_canon, &rel) {
        Ok(t) => t,
        Err(e) => return mk("refuse", false, false, e, 0, false),
    };
    let target_exists = std::fs::symlink_metadata(&target).is_ok();
    let target_hash = hash_file_hex(&target);
    let source = std::path::Path::new(&art.workspace).join(&rel);
    let source_md = std::fs::symlink_metadata(&source).ok();
    let source_size = source_md.as_ref().map(|m| m.len()).unwrap_or(0);
    let source_is_file = source_md.as_ref().map(|m| m.is_file()).unwrap_or(false);

    match kind.as_str() {
        "created" => {
            if !source_is_file {
                return mk(
                    "refuse",
                    false,
                    false,
                    "source file missing in the run workspace".into(),
                    source_size,
                    target_exists,
                );
            }
            if !target_exists {
                return mk(
                    "create",
                    true,
                    false,
                    "new file — will be created".into(),
                    source_size,
                    false,
                );
            }
            match (art.hash.as_deref(), target_hash.as_deref()) {
                (Some(s), Some(t)) if s == t => mk(
                    "noop",
                    true,
                    false,
                    "already present with identical content".into(),
                    source_size,
                    true,
                ),
                _ => mk(
                    "refuse",
                    false,
                    true,
                    "target already exists with different content".into(),
                    source_size,
                    true,
                ),
            }
        }
        "modified" => {
            if !source_is_file {
                return mk(
                    "refuse",
                    false,
                    false,
                    "source file missing in the run workspace".into(),
                    source_size,
                    target_exists,
                );
            }
            let Some(src_hash) = art.hash.as_deref() else {
                return mk(
                    "refuse",
                    false,
                    false,
                    "source too large to verify safely".into(),
                    source_size,
                    target_exists,
                );
            };
            if !target_exists {
                return mk(
                    "refuse",
                    false,
                    true,
                    "target missing — cannot safely modify".into(),
                    source_size,
                    false,
                );
            }
            let Some(tgt_hash) = target_hash.as_deref() else {
                return mk(
                    "refuse",
                    false,
                    true,
                    "target unreadable / too large to verify".into(),
                    source_size,
                    true,
                );
            };
            if tgt_hash == src_hash {
                return mk(
                    "noop",
                    true,
                    false,
                    "already updated (identical content)".into(),
                    source_size,
                    true,
                );
            }
            match art.baseline_hash.as_deref() {
                Some(base) if base == tgt_hash => mk(
                    "overwrite",
                    true,
                    false,
                    "target matches the run baseline — safe to overwrite".into(),
                    source_size,
                    true,
                ),
                _ => mk(
                    "refuse",
                    false,
                    true,
                    "target changed since the run started (or baseline unverifiable)".into(),
                    source_size,
                    true,
                ),
            }
        }
        "deleted" => {
            if !target_exists {
                return mk("noop", true, false, "already absent".into(), 0, false);
            }
            let Some(tgt_hash) = target_hash.as_deref() else {
                return mk(
                    "refuse",
                    false,
                    true,
                    "target unreadable / too large to verify before delete".into(),
                    0,
                    true,
                );
            };
            match art.baseline_hash.as_deref() {
                Some(base) if base == tgt_hash => mk(
                    "delete",
                    true,
                    false,
                    "target matches the run baseline — safe to delete".into(),
                    0,
                    true,
                ),
                _ => mk(
                    "refuse",
                    false,
                    true,
                    "target differs from the run baseline — refusing to delete".into(),
                    0,
                    true,
                ),
            }
        }
        other => mk(
            "refuse",
            false,
            false,
            format!("unknown change kind: {other}"),
            source_size,
            target_exists,
        ),
    }
}

/// Is a run safe-apply eligible? `Ok(())` when it finished cleanly
/// (`done`), the operator ACCEPTED it, and it ran in a scoped workspace;
/// `Err(reason)` otherwise. Inherit-mode / legacy / unreviewed / rejected
/// runs are all refused here — nothing of theirs is ever applied.
pub fn run_apply_eligibility(run: &super::RunRecord) -> Result<(), String> {
    if run.status != "done" {
        return Err(format!("run is `{}`, not `done`", run.status));
    }
    if run.review.as_deref() != Some("accepted") {
        return Err("run is not accepted — review and accept it before applying".to_string());
    }
    if run.workspace.is_none() {
        return Err(
            "run has no scoped workspace (inherit-mode / legacy) — nothing to apply".to_string(),
        );
    }
    Ok(())
}

/// Build the safe-apply plan for a run's artifacts against the configured
/// project root. PURE — never writes. Returns an error only when the
/// project root itself is invalid (missing / drive root).
pub fn build_apply_plan(
    project_root: &std::path::Path,
    artifacts: &[super::RunArtifact],
) -> Result<ApplyPlan, String> {
    let root_canon = validate_project_root(project_root)?;
    let mut items: Vec<ApplyPlanItem> =
        artifacts.iter().map(|a| plan_one(&root_canon, a)).collect();
    items.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    let applicable = items.iter().all(|i| i.can_apply);
    let changes = items
        .iter()
        .filter(|i| matches!(i.action, "create" | "overwrite" | "delete"))
        .count();
    let conflicts = items.iter().filter(|i| i.conflict).count();
    let blocked = items.iter().filter(|i| !i.can_apply && !i.conflict).count();
    let note = if items.is_empty() {
        "no artifacts — nothing to apply".to_string()
    } else if applicable {
        format!("{changes} change(s) ready to apply")
    } else {
        format!("refusing apply: {conflicts} conflict(s), {blocked} blocked")
    };
    Ok(ApplyPlan {
        project_root: root_canon.to_string_lossy().into_owned(),
        items,
        applicable,
        changes,
        conflicts,
        blocked,
        note,
    })
}

/// Execute a safe apply: build the plan, refuse the WHOLE run if any item is
/// unsafe (no partial apply), otherwise copy `create`/`overwrite` files and
/// remove `delete` files. Idempotent — an all-`noop` plan writes nothing.
/// Never follows symlinks; creates parent dirs as needed.
pub fn apply_run(
    project_root: &std::path::Path,
    artifacts: &[super::RunArtifact],
) -> Result<ApplyOutcome, String> {
    let plan = build_apply_plan(project_root, artifacts)?;
    if !plan.applicable {
        return Ok(ApplyOutcome {
            plan,
            status: "conflicted",
            applied_files: 0,
            failed_files: 0,
            errors: Vec::new(),
        });
    }
    let root_canon = validate_project_root(project_root)?;
    let mut applied = 0usize;
    let mut failed = 0usize;
    let mut errors: Vec<String> = Vec::new();
    for item in &plan.items {
        match item.action {
            "create" | "overwrite" => {
                let Some(art) = artifacts.iter().find(|a| a.rel_path == item.rel_path) else {
                    continue;
                };
                let target = match resolve_apply_target(&root_canon, &item.rel_path) {
                    Ok(t) => t,
                    Err(e) => {
                        failed += 1;
                        errors.push(format!("{}: {e}", item.rel_path));
                        continue;
                    }
                };
                let source = std::path::Path::new(&art.workspace).join(&item.rel_path);
                if let Some(parent) = target.parent()
                    && let Err(e) = std::fs::create_dir_all(parent)
                {
                    failed += 1;
                    errors.push(format!("{}: mkdir: {e}", item.rel_path));
                    continue;
                }
                match std::fs::copy(&source, &target) {
                    Ok(_) => applied += 1,
                    Err(e) => {
                        failed += 1;
                        errors.push(format!("{}: copy: {e}", item.rel_path));
                    }
                }
            }
            "delete" => {
                let target = match resolve_apply_target(&root_canon, &item.rel_path) {
                    Ok(t) => t,
                    Err(e) => {
                        failed += 1;
                        errors.push(format!("{}: {e}", item.rel_path));
                        continue;
                    }
                };
                match std::fs::remove_file(&target) {
                    Ok(_) => applied += 1,
                    Err(e) => {
                        failed += 1;
                        errors.push(format!("{}: delete: {e}", item.rel_path));
                    }
                }
            }
            _ => {} // noop / refuse (refuse can't occur — plan is applicable)
        }
    }
    let status = if failed > 0 { "failed" } else { "applied" };
    Ok(ApplyOutcome {
        plan,
        status,
        applied_files: applied,
        failed_files: failed,
        errors,
    })
}

/// Outcome of [`preflight_run`]: either a clear refusal (no command was
/// spawned) or a committed [`ReadyRun`].
// A transient result that is returned by value and immediately matched
// (never stored in a collection), so the variant size delta is irrelevant.
#[allow(clippy::large_enum_variant)]
pub enum Preflight {
    Refused(RunReport),
    // Boxed: a `ReadyRun` carries the pre-run workspace baseline manifest.
    Ready(Box<ReadyRun>),
}

/// Run ONE Brief synchronously through its Operative's Rig — the manual
/// "Start" action. Resolves the adapter, refuses clearly when it is
/// unavailable (never spawns), claims the Brief to block a duplicate
/// concurrent run, runs the Rig, advances the board, and chronicles the
/// result (tagged with the adapter name) exactly like the timer loop.
#[allow(clippy::too_many_arguments)]
pub fn run_brief_now(
    store: &TaskStore,
    registry: &crate::rig::RigRegistry,
    bridge_tokens: Option<&BridgeTokenStore>,
    lease_secs: i64,
    brief_id: &str,
    preferred_rig: Option<&str>,
    prompt: String,
) -> Result<RunReport, CoordinatorError> {
    match preflight_run(
        store,
        registry,
        bridge_tokens,
        lease_secs,
        brief_id,
        preferred_rig,
        prompt,
    )? {
        Preflight::Refused(report) => Ok(report),
        Preflight::Ready(ready) => Ok(execute_ready(store, bridge_tokens, *ready)),
    }
}

/// Pre-flight one Brief and, if it commits, hand the blocking adapter run to a
/// background thread — returning the immediate [`RunReport`] (`running`, with
/// a `run_id`, or a clear pre-run refusal where `run_id` is `None`).
///
/// This is the shared ASYNC core behind the manual `brief.run` handler and
/// Prime's Start-to-Shift (`prime.start`): both resolve the assignee's Rig +
/// prompt, then call this so the commit/spawn logic lives in ONE place and
/// every Shift goes through the same chokepoint
/// ([`preflight_run`] → [`prepare_claimed_run`] → [`execute_ready`]). Must be
/// called from within a Tokio runtime (it uses `spawn_blocking`). The caller
/// owns recording any tenant-scoped refusal for a `None`-`run_id` report.
#[allow(clippy::too_many_arguments)]
pub fn preflight_and_spawn(
    store: &Arc<TaskStore>,
    registry: &crate::rig::RigRegistry,
    bridge_tokens: Option<&BridgeTokenStore>,
    lease_secs: i64,
    brief_id: &str,
    preferred_rig: Option<&str>,
    prompt: String,
    prefs: RunModelPrefs,
) -> Result<RunReport, CoordinatorError> {
    // Manual provenance — delegate to the trigger-parameterized variant.
    preflight_and_spawn_with_trigger(
        store,
        registry,
        bridge_tokens,
        lease_secs,
        brief_id,
        preferred_rig,
        prompt,
        prefs,
        RunTrigger::Manual,
    )
}

/// Trigger-parameterized variant of [`preflight_and_spawn`]: same async commit +
/// background-execute shape, but stamps the committed run with `trigger`. The
/// autonomous Prime loop's bare-Mandate start passes [`RunTrigger::Heartbeat`] so
/// its Shifts read as autonomous, while still going through the ONE shared
/// [`preflight_run_with_prefs_trigger`] → [`prepare_claimed_run`] →
/// [`execute_ready`] chokepoint (claims, duplicate-run guard, adapter probe,
/// workspace prep, durable ledger, bridge token, board advancement, Chronicle).
#[allow(clippy::too_many_arguments)]
pub fn preflight_and_spawn_with_trigger(
    store: &Arc<TaskStore>,
    registry: &crate::rig::RigRegistry,
    bridge_tokens: Option<&BridgeTokenStore>,
    lease_secs: i64,
    brief_id: &str,
    preferred_rig: Option<&str>,
    prompt: String,
    prefs: RunModelPrefs,
    trigger: RunTrigger,
) -> Result<RunReport, CoordinatorError> {
    match preflight_run_with_prefs_trigger(
        store,
        registry,
        bridge_tokens,
        lease_secs,
        brief_id,
        preferred_rig,
        prompt,
        prefs,
        trigger,
    )? {
        Preflight::Refused(report) => Ok(report),
        Preflight::Ready(ready) => {
            // Committed: report `running` immediately, run the blocking adapter
            // on a background thread (a long Claude/Codex Shift must not freeze
            // the bridge). `execute_ready` advances the board, chronicles,
            // closes the ledger row, and releases the Claim.
            let accepted = RunReport {
                brief_id: ready.brief_id.clone(),
                status: "running".to_string(),
                rig: ready.rig_name.clone(),
                summary: "run started".to_string(),
                install_hint: None,
                run_id: Some(ready.run_id.clone()),
                workspace: ready.workspace.clone(),
                workspace_context: ready.workspace_context.clone(),
                workspace_files: ready.workspace_files,
                workspace_bytes: ready.workspace_bytes,
            };
            let st_bg = store.clone();
            tokio::task::spawn_blocking(move || {
                let bt = crate::rig::bridge::BridgeTokenStore::global();
                let _ = execute_ready(&st_bg, Some(&bt), *ready);
            });
            Ok(accepted)
        }
    }
}

/// Outcome of [`open_retry_child`] — the synchronous part of a guarded
/// operator retry (everything up to, but not including, the background adapter
/// run). The caller (the `run.retry` capability) maps each variant to an
/// honest response and, on [`Self::Ready`], spawns the blocking execute.
pub enum RetryOpen {
    /// The source run is unknown / cross-Guild — surfaced as not-found.
    NotFound,
    /// The source already has a retry child; carries the EXISTING child id (no
    /// second run was opened).
    AlreadyRetried { child_run_id: String },
    /// A precondition refused (ineligible source, or the shared preflight
    /// refused — adapter unavailable / `already_running` Claim conflict /
    /// workspace). Carries the structured [`RunReport`] so the caller surfaces
    /// the honest reason + status.
    Refused(RunReport),
    /// A retry child Brief run was committed (Claim won, ledger row opened,
    /// lineage stamped, retry chronicled). The caller spawns `execute_ready`.
    Ready {
        ready: Box<ReadyRun>,
        source_run_id: String,
        child_run_id: String,
        attempt: i64,
    },
}

/// Open a **guarded operator retry** of a source failed Shift
/// (execution-and-issue §3.3b / §7.4 LOCKED conservative recovery). This is the
/// synchronous chokepoint shared by the `run.retry` capability and the runtime
/// tests; it does NOT run the adapter (the caller spawns that on a background
/// thread so the bridge returns quickly).
///
/// It is a **one-click operator action, NOT a blind auto-retry loop**: the
/// runtime REFUSES unless [`TaskStore::retry_precheck`] proves the source is
/// terminal-and-failure-like, retryable, has budget, links a still-present
/// in-tenant Brief, and has no existing retry child. Eligible ⇒ it reuses the
/// EXISTING [`preflight_run`] path (same adapter resolution, Claim, workspace
/// prep, ledger row, governance) — never duplicating Rig execution logic — and
/// only AFTER the child's Claim is won + its row opened does it stamp the
/// retry lineage and chronicle `brief.retry_requested` (Brief) + `retry_started`
/// (child run). A still-running Claim by another live run surfaces as the
/// preflight `already_running` refusal (single-owner, enforced in one place).
/// Who opened a retry child — recorded on the Chronicle so a Brief's timeline
/// reads which lane retried it. The retry MECHANISM is identical for both (the
/// shared [`retry_precheck`](crate::nodes::coordinator::TaskStore::retry_precheck)
/// gate + preflight/execute path); only the provenance note differs, so the
/// autonomous lane is never a second retry path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryProvenance {
    /// A human operator clicked Retry (`run.retry`, `POST /v1/runs/:id/retry`).
    Operator,
    /// The opt-in autonomous recovery lane re-woke a retryable failed Shift
    /// (`RELIX_AUTONOMOUS_RECOVERY`, default OFF).
    Autonomous,
}

impl RetryProvenance {
    /// Human label for the Chronicle / transcript message.
    fn label(self) -> &'static str {
        match self {
            RetryProvenance::Operator => "operator",
            RetryProvenance::Autonomous => "autonomous recovery",
        }
    }

    /// The Brief Chronicle event KIND, distinct per lane so the timeline reads
    /// honestly which path retried (operator one-click vs autonomous lane).
    fn brief_event_kind(self) -> &'static str {
        match self {
            RetryProvenance::Operator => "brief.retry_requested",
            RetryProvenance::Autonomous => "brief.autonomous_retry",
        }
    }
}

/// Operator-provenance entry point — preserved verbatim for the `run.retry`
/// capability and existing callers/tests. Delegates to
/// [`open_retry_child_with_provenance`] with [`RetryProvenance::Operator`].
#[allow(clippy::too_many_arguments)]
pub fn open_retry_child(
    store: &TaskStore,
    registry: &crate::rig::RigRegistry,
    bridge_tokens: Option<&BridgeTokenStore>,
    lease_secs: i64,
    run_id: &str,
    tenant: &str,
    prompt: String,
    preferred_rig: Option<&str>,
    prefs: RunModelPrefs,
) -> Result<RetryOpen, CoordinatorError> {
    open_retry_child_with_provenance(
        store,
        registry,
        bridge_tokens,
        lease_secs,
        run_id,
        tenant,
        prompt,
        preferred_rig,
        prefs,
        RetryProvenance::Operator,
    )
}

/// Open a guarded retry of a source failed Shift, recording `provenance` on the
/// Chronicle. This is the ONE shared retry chokepoint behind BOTH the operator
/// one-click ([`open_retry_child`]) and the opt-in autonomous recovery lane
/// ([`autonomous_recovery_tick`]) — same `retry_precheck` gate, same Claim /
/// workspace / ledger / model-prefs / Codex-resume behavior, same
/// duplicate-child guard. Only the chronicled provenance differs.
#[allow(clippy::too_many_arguments)]
pub fn open_retry_child_with_provenance(
    store: &TaskStore,
    registry: &crate::rig::RigRegistry,
    bridge_tokens: Option<&BridgeTokenStore>,
    lease_secs: i64,
    run_id: &str,
    tenant: &str,
    prompt: String,
    preferred_rig: Option<&str>,
    prefs: RunModelPrefs,
    provenance: RetryProvenance,
) -> Result<RetryOpen, CoordinatorError> {
    use crate::nodes::coordinator::RetryPrecheck;
    let (brief_id, next_attempt) = match store.retry_precheck(run_id, tenant)? {
        RetryPrecheck::NotFound => return Ok(RetryOpen::NotFound),
        RetryPrecheck::AlreadyRetried { child_run_id } => {
            return Ok(RetryOpen::AlreadyRetried { child_run_id });
        }
        RetryPrecheck::Refused {
            status,
            reason,
            brief_id,
        } => {
            // Chronicle the refusal for audit when we know the Brief (best-effort).
            if let Some(b) = &brief_id {
                let _ = store.append_event(
                    b,
                    "brief.retry_refused",
                    &format!("retry of run {run_id} refused: {status} — {reason}"),
                );
            }
            let target = brief_id.as_deref().unwrap_or(run_id);
            return Ok(RetryOpen::Refused(RunReport::refuse(
                target, status, reason,
            )));
        }
        RetryPrecheck::Eligible {
            brief_id,
            next_attempt,
            ..
        } => (brief_id, next_attempt),
    };
    // Eligible: commit the child through the SHARED preflight path (adapter
    // resolution + Claim + workspace prep + ledger row + governance). A Claim
    // conflict / adapter-unavailable / workspace refusal returns a structured
    // RunReport WITHOUT opening a child row. The retry child inherits the
    // assigned Operative's stored model/effort prefs so it runs on the same
    // model the original Shift would (execution-and-issue §3.3b / adapters
    // §3.2/§3.3) — never a silent downgrade to the adapter default.
    match preflight_run_with_prefs(
        store,
        registry,
        bridge_tokens,
        lease_secs,
        &brief_id,
        preferred_rig,
        prompt,
        prefs,
    )? {
        Preflight::Refused(report) => Ok(RetryOpen::Refused(report)),
        Preflight::Ready(ready) => {
            let child_run_id = ready.run_id.clone();
            // Stamp the retry lineage on the just-opened child row (the Claim is
            // already won — never a lineage row without a run). The partial
            // UNIQUE index makes a double-link to the same source fail loudly.
            store.link_retry_child(&child_run_id, run_id, next_attempt)?;
            // Chronicle the retry on the Brief + the child run transcript so the
            // Chronicle and `/v1/runs/:id/events` both record the source + attempt
            // + which lane (operator vs autonomous) opened it.
            let _ = store.append_event(
                &brief_id,
                provenance.brief_event_kind(),
                &format!(
                    "{} retried source run {run_id} → child {child_run_id} (attempt {next_attempt})",
                    provenance.label()
                ),
            );
            let _ = store.append_run_event(
                &child_run_id,
                "retry_started",
                "relix",
                &format!(
                    "{} retry of source run {run_id} (attempt {next_attempt})",
                    provenance.label()
                ),
                None,
                false,
            );
            Ok(RetryOpen::Ready {
                ready,
                source_run_id: run_id.to_string(),
                child_run_id,
                attempt: next_attempt,
            })
        }
    }
}

// ── Stage-2 OPT-IN autonomous retry lane (execution-and-issue §3.3 / §3.3b) ──
//
// This is the autonomous side of the SAME guarded retry the operator one-click
// already drives. It is **default OFF** (`RELIX_AUTONOMOUS_RECOVERY`), **bounded
// per tick**, **idempotent** (the durable duplicate-child guard means a second
// tick never opens a second child), and **conservative**: it retries ONLY runs
// already diagnosed `retryable` with budget remaining — never a refusal, a
// budget hard-stop, a missing assignee/adapter, a permission/auth failure, a
// manual reject, a discarded run, or an exhausted-budget run. There is **no LLM
// diagnostic pass and no provider quota polling** in this slice — a run lacking
// durable diagnosis or with an ambiguous verdict is simply not retried.

/// Pure eligibility predicate for the autonomous retry lane — mirrors
/// [`retry_precheck`](crate::nodes::coordinator::TaskStore::retry_precheck)'s
/// eligibility so the lane never diverges from the guarded operator path.
/// `has_retry_child` is the durable duplicate guard (true ⇒ a child already
/// exists ⇒ never retry again). Side-effect-free and DB-free, so it is unit-
/// testable in isolation; the production selection runs the SAME logic as a
/// bounded SQL pre-filter ([`list_autonomous_retry_candidates`]).
pub fn autonomous_retry_eligible(
    run: &crate::nodes::coordinator::RunRecord,
    has_retry_child: bool,
) -> bool {
    !has_retry_child
        && matches!(run.status.as_str(), "failed" | "interrupted")
        && run.retryable == Some(true)
        && run.retry_budget_remaining.unwrap_or(0) > 0
        && !run.brief_id.trim().is_empty()
        && run.apply_status.as_deref() != Some("discarded")
}

/// Parse the opt-in autonomous-recovery switch (`RELIX_AUTONOMOUS_RECOVERY`).
/// **Default OFF** — the lane never runs unless explicitly enabled, so a fresh
/// deployment is exactly as conservative as before this slice. Accepts the same
/// truthy spellings as the heartbeat switch (`1`/`true`/`yes`/`on`).
pub fn parse_autonomous_recovery_enabled(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Parse the per-tick bound (`RELIX_AUTONOMOUS_RECOVERY_MAX`). Default 1,
/// clamped to `1..=10` so a tick can never stampede or spin — it opens at most
/// this many child retries per tick.
pub fn parse_autonomous_recovery_max(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 10)
}

/// Parse the opt-in autonomous-Prime switch (`RELIX_AUTONOMOUS_PRIME`).
/// **Default OFF** — the autonomous Prime driver never runs unless explicitly
/// enabled. Accepts the same truthy spellings as the heartbeat / recovery
/// switches (`1`/`true`/`yes`/`on`, case-insensitive, whitespace-trimmed).
pub fn parse_autonomous_prime_enabled(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Parse the per-tick bound (`RELIX_AUTONOMOUS_PRIME_MAX`) — the max number of
/// autonomous Prime actions (team-plan / orchestrate / start) per tick. Default
/// 1, clamped to `1..=10` so a tick can never stampede.
pub fn parse_autonomous_prime_max(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 10)
}

/// The per-candidate decision the recovery tick asks its caller to make. Keeps
/// the agent-store / budget policy in the caller (as [`dispatch_batch`] does
/// with its closures), so the tick itself stays decoupled and unit-testable.
pub enum RetryDecision {
    /// Do NOT retry this candidate now — the Operative is paused/terminated, its
    /// timer wake is off, or it is over budget. Quiet (no event), so the lane
    /// never spams the Chronicle every tick.
    Skip,
    /// Retry it with these resolved inputs (the assignee's Rig + charter-aware
    /// prompt + model/effort prefs — IDENTICAL to what the operator retry
    /// composes), through the shared `open_retry_child` path.
    Proceed(RetryInputs),
}

/// The resolved inputs for one autonomous retry — the same three things the
/// `run.retry` handler resolves before calling `open_retry_child`.
pub struct RetryInputs {
    pub preferred_rig: Option<String>,
    pub prompt: String,
    pub prefs: RunModelPrefs,
}

/// What the recovery tick did with one candidate (for logging / tests). The
/// lane records `opened` on the Chronicle via `open_retry_child` itself; this
/// is the in-memory tick summary, not a durable event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryRecord {
    pub source_run_id: String,
    pub brief_id: String,
    /// `opened` / `skipped` / `already_retried` / `refused` / `not_found`.
    pub outcome: &'static str,
    /// The child run id when a retry was opened (`opened` / `already_retried`).
    pub child_run_id: Option<String>,
    /// A short reason for `refused` / `skipped` (no secrets).
    pub detail: Option<String>,
}

/// Run ONE opt-in autonomous recovery tick: select up to `max` retryable failed
/// Shifts (tenant-aware) and, for each the caller approves, open EXACTLY ONE
/// child retry through the SHARED [`open_retry_child_with_provenance`] path and
/// execute it synchronously (this is called from inside `spawn_blocking`, like
/// the heartbeat dispatch loop). Returns one [`RecoveryRecord`] per candidate.
///
/// Idempotent: the duplicate-child guard inside the shared precheck means a
/// second tick over the same source returns `already_retried` and opens no
/// second run. Bounded: `max` caps work per tick. Tenant-safe: each candidate
/// carries its OWN Guild and is retried under that tenant only — `tenant=None`
/// recovers all Guilds (each under its own tenant), `tenant=Some(g)` scopes to
/// one Guild.
pub fn autonomous_recovery_tick<F>(
    store: &TaskStore,
    registry: &crate::rig::RigRegistry,
    bridge_tokens: Option<&BridgeTokenStore>,
    lease_secs: i64,
    max: usize,
    tenant: Option<&str>,
    decide: F,
) -> Result<Vec<RecoveryRecord>, CoordinatorError>
where
    F: Fn(&crate::nodes::coordinator::RetryCandidate) -> RetryDecision,
{
    if max == 0 {
        return Ok(Vec::new());
    }
    let candidates = store.list_autonomous_retry_candidates(tenant, max)?;
    let mut records = Vec::with_capacity(candidates.len());
    for cand in candidates {
        let inputs = match decide(&cand) {
            RetryDecision::Skip => {
                records.push(RecoveryRecord {
                    source_run_id: cand.run_id,
                    brief_id: cand.brief_id,
                    outcome: "skipped",
                    child_run_id: None,
                    detail: None,
                });
                continue;
            }
            RetryDecision::Proceed(inputs) => inputs,
        };
        match open_retry_child_with_provenance(
            store,
            registry,
            bridge_tokens,
            lease_secs,
            &cand.run_id,
            &cand.tenant,
            inputs.prompt,
            inputs.preferred_rig.as_deref(),
            inputs.prefs,
            RetryProvenance::Autonomous,
        )? {
            RetryOpen::NotFound => records.push(RecoveryRecord {
                source_run_id: cand.run_id,
                brief_id: cand.brief_id,
                outcome: "not_found",
                child_run_id: None,
                detail: None,
            }),
            RetryOpen::AlreadyRetried { child_run_id } => records.push(RecoveryRecord {
                source_run_id: cand.run_id,
                brief_id: cand.brief_id,
                outcome: "already_retried",
                child_run_id: Some(child_run_id),
                detail: None,
            }),
            RetryOpen::Refused(report) => records.push(RecoveryRecord {
                source_run_id: cand.run_id,
                brief_id: cand.brief_id,
                outcome: "refused",
                child_run_id: None,
                detail: Some(report.summary),
            }),
            RetryOpen::Ready {
                ready,
                child_run_id,
                ..
            } => {
                // Committed: run the blocking adapter synchronously (the tick
                // runs inside `spawn_blocking`, exactly like `dispatch_batch`).
                let _ = execute_ready(store, bridge_tokens, *ready);
                records.push(RecoveryRecord {
                    source_run_id: cand.run_id,
                    brief_id: cand.brief_id,
                    outcome: "opened",
                    child_run_id: Some(child_run_id),
                    detail: None,
                });
            }
        }
    }
    Ok(records)
}

/// The assigned Operative's stored per-run model hints, threaded from the
/// agent profile into the run request (relix-agent-adapters.md §3.2/§3.3).
/// Both fields are optional; an absent / blank preference means "use the
/// adapter's own default model," and a Rig that doesn't support the hint
/// (echo / raw / Gemini) simply ignores it. Default = neither set, which is
/// the backward-compatible behavior every existing caller gets for free.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunModelPrefs {
    /// `agent_profiles.model_preference` (e.g. `claude-sonnet-4`,
    /// `gpt-5-codex`) → the adapter's `--model` flag.
    pub model: Option<String>,
    /// `agent_profiles.reasoning_effort` (`minimal`/`low`/`medium`/`high`)
    /// → Codex's `-c model_reasoning_effort=<effort>`.
    pub effort: Option<String>,
}

impl RunModelPrefs {
    pub fn new(model: Option<String>, effort: Option<String>) -> Self {
        Self { model, effort }
    }
}

/// Pre-flight one Brief run with NO per-run model hints — the backward-
/// compatible entry. Thin wrapper over [`preflight_run_with_prefs`]; the
/// production manual path ([`preflight_and_spawn`]) calls the with-prefs
/// variant so a configured Operative's model preference reaches its Rig.
#[allow(clippy::too_many_arguments)]
pub fn preflight_run(
    store: &TaskStore,
    registry: &crate::rig::RigRegistry,
    bridge_tokens: Option<&BridgeTokenStore>,
    lease_secs: i64,
    brief_id: &str,
    preferred_rig: Option<&str>,
    prompt: String,
) -> Result<Preflight, CoordinatorError> {
    preflight_run_with_prefs(
        store,
        registry,
        bridge_tokens,
        lease_secs,
        brief_id,
        preferred_rig,
        prompt,
        RunModelPrefs::default(),
    )
}

/// Pre-flight one Brief run: resolve the Operative's Rig, refuse clearly
/// when it is unavailable (never spawns), and — only once the run is
/// committed (adapter available + Claim won) — open the durable run
/// record, advance the board, and mint the scoped bridge-back token.
/// `prefs` carries the assigned Operative's stored model/effort hints into
/// the run request. Returns a [`ReadyRun`] the caller can execute
/// synchronously or hand to a background thread (async dispatch).
#[allow(clippy::too_many_arguments)]
pub fn preflight_run_with_prefs(
    store: &TaskStore,
    registry: &crate::rig::RigRegistry,
    bridge_tokens: Option<&BridgeTokenStore>,
    lease_secs: i64,
    brief_id: &str,
    preferred_rig: Option<&str>,
    prompt: String,
    prefs: RunModelPrefs,
) -> Result<Preflight, CoordinatorError> {
    // Manual provenance (dashboard `brief.run` / sovereign `prime.start`) —
    // delegates to the trigger-parameterized core, stamping `RunTrigger::Manual`.
    preflight_run_with_prefs_trigger(
        store,
        registry,
        bridge_tokens,
        lease_secs,
        brief_id,
        preferred_rig,
        prompt,
        prefs,
        RunTrigger::Manual,
    )
}

/// Trigger-parameterized core of [`preflight_run_with_prefs`]. The pre-flight is
/// IDENTICAL (Rig resolution + availability probe, the per-Operative start lock,
/// the duplicate-start / live-run guard, the single-owner two-pointer Claim,
/// terminal-Claim reclaim, scoped workspace prep, the durable `brief_runs` ledger
/// row, the scoped bridge-back token, and board advancement) — only the committed
/// run's `trigger` differs. The AUTONOMOUS Prime loop's bare-Mandate start passes
/// [`RunTrigger::Heartbeat`] so its runs read as autonomous/heartbeat-style rather
/// than dashboard-`manual`, while going through this ONE chokepoint. Existing
/// manual callers keep using [`preflight_run_with_prefs`] and are unchanged.
#[allow(clippy::too_many_arguments)]
pub fn preflight_run_with_prefs_trigger(
    store: &TaskStore,
    registry: &crate::rig::RigRegistry,
    bridge_tokens: Option<&BridgeTokenStore>,
    lease_secs: i64,
    brief_id: &str,
    preferred_rig: Option<&str>,
    prompt: String,
    prefs: RunModelPrefs,
    trigger: RunTrigger,
) -> Result<Preflight, CoordinatorError> {
    let Some(card) = store.brief_card(brief_id)? else {
        return Ok(Preflight::Refused(RunReport::refuse(
            brief_id,
            "not_found",
            "brief not found",
        )));
    };
    let Some(assignee) = card.assignee_agent_id.clone() else {
        return Ok(Preflight::Refused(RunReport::refuse(
            brief_id,
            "unassigned",
            "assign an Operative before running",
        )));
    };
    let Some(rig) = registry.resolve(preferred_rig) else {
        return Ok(Preflight::Refused(RunReport::refuse(
            brief_id,
            "no_adapter",
            "the Operative has no Rig and no Guild default is configured",
        )));
    };
    // Live availability probe — never spawn an adapter that isn't there.
    let probe = rig.probe();
    if probe.status != "available" {
        return Ok(Preflight::Refused(RunReport {
            brief_id: brief_id.to_string(),
            status: "adapter_unavailable".to_string(),
            rig: rig.name().to_string(),
            summary: probe.detail,
            install_hint: probe.install_hint,
            run_id: None,
            workspace: None,
            workspace_context: None,
            workspace_files: None,
            workspace_bytes: None,
        }));
    }
    // PHASE 3 start lock (execution §2.6): serialize concurrent start passes
    // for the SAME Operative across the check-then-claim critical section
    // below, so two passes can't both claim a run slot / double-start this
    // Operative/Brief execution path. Distinct from the per-Brief DB Claim and
    // from the live-run count. Different Operatives lock independently, so
    // unrelated work proceeds in parallel. Held only across this synchronous
    // claim+commit (never across the adapter run), so it cannot go stale.
    let start_lock = store.agent_start_lock(&assignee);
    let _start_guard = start_lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Duplicate-start guard (execution-and-issue-design §1.4 idempotent
    // self-ownership / §2.6 one run per issue): `claim_brief_for_run`
    // intentionally lets the SAME Operative REFRESH a live Claim (so the
    // wakeup/heartbeat/lease paths stay idempotent), and the start path always
    // mints a NEW run id — so WITHOUT this guard, two manual/Prime starts by the
    // same Operative for the same Brief would BOTH be accepted, opening
    // duplicate run rows/workspaces. While holding the start lock (so concurrent
    // same-Operative starts serialize and the loser observes the winner's run),
    // refuse a new start when this Operative already has a LIVE, actually-running
    // run on the Brief. (A stale Claim with no running run is NOT a duplicate —
    // reclaiming that is stale-run adoption, a separate slice — so it does not
    // match here.) The conflict surfaces as `already_running` → HTTP 409; the
    // client must NEVER retry a 409 while the holder is live.
    if store.live_run_by_agent(&card.task_id, &assignee)?.is_some() {
        return Ok(Preflight::Refused(RunReport {
            brief_id: brief_id.to_string(),
            status: "already_running".to_string(),
            rig: rig.name().to_string(),
            summary: "this Operative already has a live run on this Brief".to_string(),
            install_hint: None,
            run_id: None,
            workspace: None,
            workspace_context: None,
            workspace_files: None,
            workspace_bytes: None,
        }));
    }
    // Stale-run adoption by terminal evidence (execution-and-issue-design §1.4
    // "stale-run adoption" / §7.1 LOCKED two-pointer Claim): if a prior Shift
    // left a LIVE Claim pointing at a run that has already reached a terminal
    // state, reclaim it NOW so this start isn't stuck behind a dead owner until
    // the lease ages out (`recover_stale_runs` can't help — it only sweeps
    // `running` rows). Safe by construction: it only releases on terminal
    // evidence matching the Claim's own pointer, never a Claim that backs a
    // still-`running` run or a Claim a newer run has re-acquired (see
    // `reclaim_terminal_claim`). The duplicate-start guard above already passed
    // (no `running` run by THIS Operative), so a terminal Claim is the only
    // thing the claim below could collide with — clear it before claiming.
    let _ = store.reclaim_terminal_claim(&card.task_id)?;
    // Single-owner: claim the Brief so a duplicate concurrent run can't
    // start. A live claim by another run → refuse.
    let run_id = format!("run_{}", uuid::Uuid::new_v4());
    if !store.claim_brief_for_run(&card.task_id, &assignee, lease_secs, Some(&run_id))? {
        return Ok(Preflight::Refused(RunReport {
            brief_id: brief_id.to_string(),
            status: "already_running".to_string(),
            rig: rig.name().to_string(),
            summary: "another run holds the Claim on this Brief".to_string(),
            install_hint: None,
            run_id: None,
            workspace: None,
            workspace_context: None,
            workspace_files: None,
            workspace_bytes: None,
        }));
    }
    // Commit the run through the SHARED pipeline (workspace, ledger row,
    // transcript, token, baseline). A workspace-prep refusal releases the
    // just-won Claim (no phantom run, no repo-wide fallback).
    match prepare_claimed_run(
        store,
        bridge_tokens,
        lease_secs,
        &card,
        &assignee,
        rig,
        &run_id,
        prompt,
        trigger,
        prefs,
    )? {
        Ok(ready) => Ok(Preflight::Ready(Box::new(ready))),
        Err(report) => {
            let _ = store.release_claim(&card.task_id, &assignee);
            Ok(Preflight::Refused(report))
        }
    }
}

/// Commit an already-CLAIMED Brief into a [`ReadyRun`]: prepare the scoped
/// workspace, advance the board, open the durable `brief_runs` ledger row
/// (stamped with `trigger`), register cancellation, write the lifecycle
/// transcript events, mint the scoped bridge-back token, and snapshot the
/// pre-run workspace baseline. This is the ONE place a run is committed —
/// BOTH the manual path ([`preflight_run`]) and the autonomous heartbeat
/// dispatch call it, so every execution produces the same durable output.
///
/// The caller must already hold the Brief's Claim and have confirmed the
/// adapter is available. Returns:
///   - `Ok(Ok(ReadyRun))` — committed; hand to [`execute_ready`];
///   - `Ok(Err(RunReport))` — a workspace-prep refusal (NO ledger row was
///     opened, board untouched). The caller cleans up its Claim / wakeup.
#[allow(clippy::too_many_arguments)]
pub fn prepare_claimed_run(
    store: &TaskStore,
    bridge_tokens: Option<&BridgeTokenStore>,
    lease_secs: i64,
    card: &brief::BriefCard,
    assignee: &str,
    rig: std::sync::Arc<dyn Rig>,
    run_id: &str,
    prompt: String,
    trigger: RunTrigger,
    prefs: RunModelPrefs,
) -> Result<Result<ReadyRun, RunReport>, CoordinatorError> {
    let rig_name = rig.name().to_string();
    // Scoped per-run workspace — the Rig executes HERE, not in the
    // coordinator/repo CWD, unless `inherit` mode is explicitly set. The
    // path is derived only from `run_id` (a prompt can't choose it). With
    // `copy_repo` context a capped, filtered project snapshot is copied in.
    // ANY prep failure refuses cleanly WITHOUT opening a run record (no
    // phantom run, no repo-wide fallback): a basic-creation failure →
    // `workspace_error`, a context/cap failure → `workspace_context_error`.
    let mut workspace: Option<String> = None;
    let mut workspace_context: Option<String> = None;
    let mut workspace_files: Option<i64> = None;
    let mut workspace_bytes: Option<i64> = None;
    if !workspace_mode_is_inherit() {
        match prepare_run_workspace(
            store.run_workspace_root(),
            run_id,
            &card.task_id,
            &card.title,
            &brief_context(card),
            store.run_workspace_config(),
        ) {
            Ok(prepared) => {
                workspace = Some(prepared.path.to_string_lossy().into_owned());
                workspace_context = Some(prepared.context.as_str().to_string());
                workspace_files = Some(prepared.copied_files as i64);
                workspace_bytes = Some(prepared.copied_bytes as i64);
            }
            Err(e) => {
                return Ok(Err(RunReport {
                    brief_id: card.task_id.clone(),
                    status: e.status().to_string(),
                    rig: rig_name,
                    summary: format!("could not prepare a run workspace: {}", e.message()),
                    install_hint: None,
                    run_id: None,
                    workspace: None,
                    workspace_context: None,
                    workspace_files: None,
                    workspace_bytes: None,
                }));
            }
        }
    }
    if card.board_status == "todo" {
        store.set_board_status(&card.task_id, "in_progress")?;
    }
    // Durable run record (status `running`) — the dashboard polls this.
    let _ = store.record_run_start(
        run_id,
        &card.task_id,
        assignee,
        &rig_name,
        trigger.as_str(),
        &crate::nodes::coordinator::RunWorkspaceInfo {
            path: workspace.as_deref(),
            context: workspace_context.as_deref(),
            files: workspace_files,
            bytes: workspace_bytes,
        },
    );
    // Stamp the run's effective billing code at START (company-model §6.6):
    // the Brief's own code, else inherited from the nearest same-Guild
    // ancestor Sub-brief. Durable point-in-time attribution — a later change
    // to the Brief's code never rewrites this run's bill. Best-effort + a
    // no-op when there is no code (the column stays NULL → unattributed).
    let _ = store.stamp_run_billing_code(run_id, &card.task_id);
    // Register the run as cancellable + open its transcript with the
    // lifecycle events an operator needs to follow the run.
    crate::rig::CancelRegistry::global().register(run_id);
    let _ = store.append_run_event(
        run_id,
        "accepted",
        "relix",
        &format!(
            "run accepted on adapter `{rig_name}` ({} trigger)",
            trigger.as_str()
        ),
        None,
        false,
    );
    let ws_msg = match &workspace {
        Some(ws) => format!(
            "workspace ready ({}): {}",
            workspace_context.as_deref().unwrap_or("empty"),
            ws
        ),
        None => "running in the coordinator working directory (inherit mode)".to_string(),
    };
    let _ = store.append_run_event(run_id, "workspace_prepared", "relix", &ws_msg, None, false);
    let _ = store.append_event(
        &card.task_id,
        "brief.run_started",
        &format!("[{rig_name}] {} run {run_id}", trigger.as_str()),
    );
    // Scoped per-run bridge-back token (dies with the run).
    let token = bridge_tokens
        .map(|bt| {
            bt.mint_scoped(
                &card.task_id,
                assignee,
                "",
                lease_secs,
                BRIDGE_BACK_SHIFT_METHODS
                    .iter()
                    .map(|m| (*m).to_string())
                    .collect(),
            )
        })
        .unwrap_or_default();
    // Look up a SAFE, same-scope resumable adapter session so a subscription
    // CLI Operative continues its prior thread instead of starting cold. The
    // lookup is keyed on EXACTLY (tenant, Operative, Rig, Brief) — the same
    // 4-tuple `record_run_runtime_state` writes — so a session stored under a
    // different tenant, Operative, Rig, or unrelated Brief can never match.
    // Only a supported CLI Rig (Codex) actually maps the id to a resume argv;
    // every other Rig ignores the field (relix-agent-adapters.md §3.3).
    // Best-effort — any lookup miss/error simply runs fresh.
    let resume_session = store
        .resume_session_for(&card.task_id, assignee, &rig_name)
        .ok()
        .flatten();
    let mut req = RigRunRequest::new(&card.task_id, assignee, String::new(), prompt)
        .with_run_id(run_id)
        .with_bridge_token(&token)
        .with_context(brief_context(card))
        // Carry the assigned Operative's stored model/effort preference into
        // the run; a supported CLI Rig maps it to its `--model` /
        // `-c model_reasoning_effort` flags, others ignore it (relix-agent-
        // adapters.md §3.2/§3.3). Empty/absent normalizes away in the builder.
        .with_model_preference(prefs.model.clone())
        .with_reasoning_effort(prefs.effort.clone())
        // Continue the same-scope adapter session when one is stored (Codex
        // only maps it; others ignore). Absent/blank normalizes away.
        .with_resume_session_id(resume_session);
    // Pin the child's working directory to the scoped workspace.
    if let Some(ws) = &workspace {
        req = req.with_working_dir(std::path::PathBuf::from(ws));
    }
    // Snapshot the workspace BEFORE the run so we can detect what the agent
    // changed (created/modified/deleted) when it finishes.
    let baseline = match &workspace {
        Some(ws) => scan_workspace_manifest(std::path::Path::new(ws)),
        None => WorkspaceManifest::default(),
    };
    Ok(Ok(ReadyRun {
        brief_id: card.task_id.clone(),
        assignee: assignee.to_string(),
        run_id: run_id.to_string(),
        rig_name,
        workspace,
        workspace_context,
        workspace_files,
        workspace_bytes,
        baseline,
        rig,
        req,
        token,
    }))
}

/// Execute a [`ReadyRun`]: run the Rig (blocking), advance the board,
/// chronicle the result (tagged with the adapter name), close the
/// durable run record, and release the Claim. The blocking call here is
/// what async dispatch moves onto a background thread.
pub fn execute_ready(
    store: &TaskStore,
    bridge_tokens: Option<&BridgeTokenStore>,
    ready: ReadyRun,
) -> RunReport {
    execute_ready_inner(store, bridge_tokens, ready).0
}

/// Like [`execute_ready`] but also returns the terminal [`RigOutcome`] so
/// the autonomous dispatcher can build an accurate `DispatchRecord`
/// (preserving the `retryable` distinction the `RunReport` status flattens).
fn execute_ready_inner(
    store: &TaskStore,
    bridge_tokens: Option<&BridgeTokenStore>,
    ready: ReadyRun,
) -> (RunReport, RigOutcome) {
    let ReadyRun {
        brief_id,
        assignee,
        run_id,
        rig_name,
        workspace,
        workspace_context,
        workspace_files,
        workspace_bytes,
        baseline,
        rig,
        req,
        token,
    } = ready;
    let _ = store.append_run_event(
        &run_id,
        "process_started",
        "relix",
        &format!("executing on adapter `{rig_name}`"),
        None,
        false,
    );
    // Run the Rig AND collect its transcript events (parsed adapter
    // output). The blocking call here is what async dispatch moves onto a
    // background thread; the wait loop polls the cancel flag.
    let run = rig.run_transcript(&req);
    let outcome = run.outcome;
    // Persist the usage/cost/session parsed from the adapter output (TG6);
    // a no-op when nothing was captured (echo / raw / no usage).
    let _ = store.set_run_usage(&run_id, &run.usage);
    // Persist the parsed adapter events (already redacted + bounded).
    for ev in &run.events {
        let _ = store.append_run_event(
            &run_id,
            &ev.kind,
            &ev.source,
            &ev.message,
            ev.payload_json.as_deref(),
            true,
        );
    }
    if let Some(bt) = bridge_tokens
        && !token.is_empty()
    {
        bt.revoke(&token);
    }
    // A cancel request that landed mid-run wins over the raw outcome: the
    // process was killed, so report the run `cancelled`, not `failed`.
    let was_cancelled = crate::rig::CancelRegistry::global().is_cancelled(&run_id);
    crate::rig::CancelRegistry::global().clear(&run_id);
    let _ = store.append_run_event(
        &run_id,
        "process_exited",
        "relix",
        "process exited",
        None,
        false,
    );

    // Detect what the agent changed in the scoped workspace (the
    // reviewable result). Only scopes-runs are scanned — `inherit` mode
    // has no scoped dir and we NEVER scan the repo. Failures are surfaced
    // as a transcript event, never swallowed silently.
    if let Some(ws) = &workspace {
        let _ = store.append_run_event(
            &run_id,
            "artifacts.scan_started",
            "relix",
            "scanning the workspace for changes",
            None,
            false,
        );
        let after = scan_workspace_manifest(std::path::Path::new(ws));
        let changes = diff_manifests(&baseline, &after);
        let total = changes.len();
        let mut recorded = 0usize;
        let mut truncated = false;
        for ch in &changes {
            match store.record_run_artifact(
                &run_id,
                &brief_id,
                ws,
                &ch.rel_path,
                ch.kind,
                ch.size as i64,
                ch.hash.as_deref(),
                ch.baseline_hash.as_deref(),
                ch.is_text,
            ) {
                Ok(true) => recorded += 1,
                Ok(false) => {
                    truncated = true;
                    break;
                }
                Err(e) => {
                    let _ = store.append_run_event(
                        &run_id,
                        "artifacts.scan_failed",
                        "relix",
                        &format!("could not record an artifact: {e}"),
                        None,
                        false,
                    );
                    truncated = true;
                    break;
                }
            }
        }
        let created = changes.iter().filter(|c| c.kind == "created").count();
        let modified = changes.iter().filter(|c| c.kind == "modified").count();
        let deleted = changes.iter().filter(|c| c.kind == "deleted").count();
        let note = if truncated || after.truncated {
            format!(
                "{recorded}/{total} change(s) recorded (truncated): {created} created, {modified} modified, {deleted} deleted"
            )
        } else {
            format!("{total} change(s): {created} created, {modified} modified, {deleted} deleted")
        };
        let _ = store.append_run_event(&run_id, "artifacts.detected", "relix", &note, None, false);
    }

    let (status, summary) = if was_cancelled {
        let _ = store.set_board_status(&brief_id, "blocked");
        let _ = store.append_event(
            &brief_id,
            "brief.dispatch_failed",
            &format!("[{rig_name}] cancelled by operator"),
        );
        (
            "cancelled",
            "run cancelled by operator (process killed)".to_string(),
        )
    } else {
        match &outcome {
            RigOutcome::Done { summary } => {
                // Done → in_review (best-effort; a missing reviewer parks it).
                if store.set_board_status(&brief_id, "in_review").is_err() {
                    let _ = store.set_board_status(&brief_id, "blocked");
                }
                let _ = store.append_event(
                    &brief_id,
                    "brief.shift_done",
                    &format!("[{rig_name}] {summary}"),
                );
                ("done", summary.clone())
            }
            RigOutcome::Failed {
                retryable: false,
                reason,
            } => {
                let _ = store.set_board_status(&brief_id, "blocked");
                let _ = store.append_event(
                    &brief_id,
                    "brief.dispatch_failed",
                    &format!("[{rig_name}] {reason}"),
                );
                ("failed", reason.clone())
            }
            RigOutcome::Failed {
                retryable: true,
                reason,
            } => {
                let _ = store.append_event(
                    &brief_id,
                    "brief.dispatch_failed",
                    &format!("[{rig_name}] {reason}"),
                );
                ("failed", reason.clone())
            }
            RigOutcome::Continue { note } => {
                let _ = store.append_event(
                    &brief_id,
                    "brief.continued",
                    &format!("[{rig_name}] {note}"),
                );
                ("continued", note.clone())
            }
        }
    };
    // Terminal lifecycle transcript event.
    let term_kind = match status {
        "done" => "result",
        "cancelled" => "cancelled",
        "continued" => "continued",
        _ => "failed",
    };
    let _ = store.append_run_event(&run_id, term_kind, "relix", &summary, None, true);
    let _ = store.record_run_finish(&run_id, status, &summary);
    // A real Rig `failed` carries the `RigOutcome` retryable signal that
    // `record_run_finish` (status-only) can't see — re-stamp the diagnosis with
    // it so a transient (timeout) failure reads `retryable` and a hard
    // (governance / permanent / auth / config) failure reads non-retryable
    // (execution-and-issue §3.3b). `done` / `continued` / `cancelled` /
    // `interrupted` are already classified honestly from the status alone.
    if status == "failed" {
        let rig_retryable = matches!(
            &outcome,
            RigOutcome::Failed {
                retryable: true,
                ..
            }
        );
        let diag = super::RunDiagnosis::for_terminal(status, Some(rig_retryable), &run_id);
        let _ = store.set_run_diagnosis(&run_id, &diag);
    }
    // Persist per-(tenant, agent, rig, brief) adapter runtime state (TG2): the
    // resumable session id, accumulated usage/cost, and the latest run status.
    // `last_error` is the failure reason on a non-success terminal state, else
    // cleared — honest latest-error semantics. Best-effort.
    let last_error = if matches!(status, "failed" | "cancelled") {
        Some(summary.as_str())
    } else {
        None
    };
    let _ = store.record_run_runtime_state(&run_id, &run.usage, status, last_error, None);
    let _ = store.release_claim(&brief_id, &assignee);
    (
        RunReport {
            brief_id,
            status: status.to_string(),
            rig: rig_name,
            summary,
            install_hint: None,
            run_id: Some(run_id),
            workspace,
            workspace_context,
            workspace_files,
            workspace_bytes,
        },
        outcome,
    )
}

/// Build the opaque `context` string handed to the Rig: where the
/// Brief sits on the spine (priority + Mandate/Campaign links), so
/// the agent backend knows the work's place in the company without
/// a separate lookup.
fn brief_context(card: &brief::BriefCard) -> String {
    let mut parts = vec![format!("priority={}", card.priority)];
    if let Some(m) = &card.mandate_id {
        parts.push(format!("mandate={m}"));
    }
    if let Some(c) = &card.campaign_id {
        parts.push(format!("campaign={c}"));
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::coordinator::RetryPolicy;

    fn store() -> TaskStore {
        TaskStore::in_memory().unwrap()
    }

    /// A store whose scoped-run workspaces land in a fresh tempdir (held
    /// by the returned guard) so a real `preflight_run` creates + cleans
    /// its workspaces deterministically, without touching the global temp.
    fn store_ws() -> (TaskStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = TaskStore::in_memory().unwrap();
        s.set_run_workspace_root(tmp.path().join("runs"));
        (s, tmp)
    }

    fn ready_brief(s: &TaskStore, title: &str, assignee: &str) -> String {
        let id = s
            .create(
                title,
                "flows/none.sol",
                "{}",
                "subj",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        s.set_brief_field(&id, "assignee", assignee).unwrap();
        s.set_brief_field(&id, "reviewer", "reviewer_1").unwrap();
        s.set_board_status(&id, "todo").unwrap();
        id
    }

    #[test]
    fn claim_ready_batch_claims_each_ready_brief_once_for_its_assignee() {
        let s = store();
        let a = ready_brief(&s, "a", "agt_a");
        let b = ready_brief(&s, "b", "agt_b");

        // First tick claims both, for their respective assignees.
        let first = claim_ready_batch(&s, 50, 300).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(s.claim_holder(&a).unwrap().unwrap().0, "agt_a");
        assert_eq!(s.claim_holder(&b).unwrap().unwrap().0, "agt_b");

        // Second tick wins nothing — both are held by a live Claim.
        assert!(claim_ready_batch(&s, 50, 300).unwrap().is_empty());
    }

    #[test]
    fn claim_ready_batch_skips_unassigned_blocked_and_done() {
        let s = store();
        let live = ready_brief(&s, "live", "agt_x");

        // Unassigned: not ready, never dispatched.
        let unassigned = s
            .create(
                "u",
                "flows/none.sol",
                "{}",
                "subj",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        s.set_board_status(&unassigned, "todo").unwrap();

        // Blocked: ready query excludes it.
        let blocked = ready_brief(&s, "blocked", "agt_y");
        let blocker = s
            .create(
                "blk",
                "flows/none.sol",
                "{}",
                "subj",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        s.add_snag(&blocked, &blocker).unwrap();

        let dispatched: Vec<String> = claim_ready_batch(&s, 50, 300)
            .unwrap()
            .into_iter()
            .map(|c| c.task_id)
            .collect();
        assert!(dispatched.contains(&live));
        assert!(!dispatched.contains(&unassigned));
        assert!(!dispatched.contains(&blocked));
    }

    #[test]
    fn an_expired_lease_lets_the_next_tick_reclaim() {
        let s = store();
        let id = ready_brief(&s, "a", "agt_a");
        assert_eq!(claim_ready_batch(&s, 50, 300).unwrap().len(), 1);

        // Backdate the lease into the past — the dispatcher "crashed".
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE tasks SET claim_expires_at = 100 WHERE task_id = ?1",
                rusqlite::params![id],
            )
            .unwrap();
        }
        // The next tick reclaims and re-dispatches it.
        let again: Vec<String> = claim_ready_batch(&s, 50, 300)
            .unwrap()
            .into_iter()
            .map(|c| c.task_id)
            .collect();
        assert!(again.contains(&id));
    }

    #[test]
    fn dispatch_batch_runs_each_brief_on_its_rig_and_advances_the_board() {
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let reg = RigRegistry::with_builtins();
        let a = ready_brief(&s, "write docs", "agt_a"); // starts in todo

        let records = dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| reg.get("echo"),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].brief_id, a);
        assert_eq!(records[0].rig, "echo");
        assert!(matches!(records[0].outcome, RigOutcome::Done { .. }));

        // Board advanced todo → in_progress → in_review; Claim released.
        assert_eq!(s.board_status(&a).unwrap().as_deref(), Some("in_review"));
        assert!(s.claim_holder(&a).unwrap().is_none());
        // The Shift result was chronicled for the reviewer.
        let done = s
            .query_events(
                &a,
                0,
                50,
                Some("brief.shift_done"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert_eq!(done.len(), 1);
        assert!(
            done[0].payload.contains("write docs"),
            "got {:?}",
            done[0].payload
        );
        // No longer ready, so a second tick does nothing.
        assert!(s.list_ready_briefs(50).unwrap().is_empty());
        assert!(
            dispatch_batch(
                &s,
                50,
                300,
                None,
                |_: &brief::BriefCard| reg.get("echo"),
                |c: &brief::BriefCard| c.title.clone(),
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn dispatch_policy_can_disable_timer_wake_for_ready_brief() {
        use crate::rig::RigRegistry;
        let s = store();
        let reg = RigRegistry::with_builtins();
        let a = ready_brief(&s, "do not wake", "agt_a");

        let records = dispatch_batch_with_policy(
            &s,
            50,
            300,
            None,
            |_| false,
            |_| 20,
            |_| BudgetAdmission::Allow,
            |_: &brief::BriefCard| reg.get("echo"),
            |c: &brief::BriefCard| c.title.clone(),
            |_: &brief::BriefCard| RunModelPrefs::default(),
        )
        .unwrap();
        assert!(records.is_empty());
        assert_eq!(s.board_status(&a).unwrap().as_deref(), Some("todo"));
        assert!(s.claim_holder(&a).unwrap().is_none());
        assert!(s.list_brief_wakeups(&a, 10).unwrap().is_empty());
    }

    #[test]
    fn dispatch_policy_honors_per_agent_concurrency_cap() {
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let reg = RigRegistry::with_builtins();
        let a = ready_brief(&s, "a", "agt_a");
        let b = ready_brief(&s, "b", "agt_a");

        let records = dispatch_batch_with_policy(
            &s,
            50,
            300,
            None,
            |_| true,
            |agent| if agent == "agt_a" { 1 } else { 20 },
            |_| BudgetAdmission::Allow,
            |_: &brief::BriefCard| reg.get("echo"),
            |c: &brief::BriefCard| c.title.clone(),
            |_: &brief::BriefCard| RunModelPrefs::default(),
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        let done_id = records[0].brief_id.clone();
        let queued_id = if done_id == a { b } else { a };
        assert_eq!(
            s.board_status(&done_id).unwrap().as_deref(),
            Some("in_review")
        );
        assert_eq!(s.board_status(&queued_id).unwrap().as_deref(), Some("todo"));
        let queued_rows = s.list_brief_wakeups(&queued_id, 10).unwrap();
        assert_eq!(queued_rows.len(), 1);
        assert_eq!(queued_rows[0].status, "queued");
    }

    // ── Per-Operative model preference reaches the Rig request ──

    /// A Rig that records the model/effort hints on the request it is asked
    /// to run, so a test can prove the stored preference flows through the
    /// dispatch chokepoint into the [`RigRunRequest`].
    type CapturedRigHints =
        std::sync::Arc<std::sync::Mutex<Option<(Option<String>, Option<String>)>>>;

    struct CaptureRig {
        seen: CapturedRigHints,
    }
    impl Rig for CaptureRig {
        fn name(&self) -> &str {
            "capture"
        }
        fn run(&self, req: &RigRunRequest) -> RigOutcome {
            *self.seen.lock().unwrap() =
                Some((req.model_preference.clone(), req.reasoning_effort.clone()));
            RigOutcome::Done {
                summary: "captured".to_string(),
            }
        }
    }

    #[test]
    fn dispatch_passes_operative_model_prefs_into_run_request() {
        let (s, _tmp) = store_ws();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let rig: Arc<dyn Rig> = Arc::new(CaptureRig { seen: seen.clone() });
        ready_brief(&s, "cap", "agt_cap");
        let records = dispatch_batch_with_policy(
            &s,
            50,
            300,
            None,
            |_| true,
            |_| 20,
            |_| BudgetAdmission::Allow,
            |_: &brief::BriefCard| Some(rig.clone()),
            |c: &brief::BriefCard| c.title.clone(),
            // Stand in for the controller's agent-profile lookup.
            |_: &brief::BriefCard| {
                RunModelPrefs::new(Some("gpt-5-codex".to_string()), Some("high".to_string()))
            },
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        let got = seen.lock().unwrap().clone().expect("the rig ran");
        assert_eq!(
            got.0.as_deref(),
            Some("gpt-5-codex"),
            "model pref reached the request"
        );
        assert_eq!(got.1.as_deref(), Some("high"), "effort reached the request");
    }

    #[test]
    fn dispatch_omits_model_prefs_when_operative_has_none() {
        // No stored preference → the request carries neither hint (the Rig
        // runs on its own default model). Proves the absent path stays clean.
        let (s, _tmp) = store_ws();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let rig: Arc<dyn Rig> = Arc::new(CaptureRig { seen: seen.clone() });
        ready_brief(&s, "cap2", "agt_cap2");
        let records = dispatch_batch_with_policy(
            &s,
            50,
            300,
            None,
            |_| true,
            |_| 20,
            |_| BudgetAdmission::Allow,
            |_: &brief::BriefCard| Some(rig.clone()),
            |c: &brief::BriefCard| c.title.clone(),
            |_: &brief::BriefCard| RunModelPrefs::default(),
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        let got = seen.lock().unwrap().clone().expect("the rig ran");
        assert_eq!(got.0, None, "no model pref when the Operative has none");
        assert_eq!(got.1, None, "no effort when the Operative has none");
    }

    // ── Same-scope adapter session resume reaches the Rig request ──

    /// A Rig that records the resume session id on the request it is asked to
    /// run, so a test can prove a stored compatible session reaches the
    /// dispatch chokepoint's [`RigRunRequest`] (and an incompatible one does
    /// not).
    struct ResumeCaptureRig {
        seen: std::sync::Arc<std::sync::Mutex<Option<Option<String>>>>,
    }
    impl Rig for ResumeCaptureRig {
        fn name(&self) -> &str {
            "capture"
        }
        fn run(&self, req: &RigRunRequest) -> RigOutcome {
            *self.seen.lock().unwrap() = Some(req.resume_session_id.clone());
            RigOutcome::Done {
                summary: "captured".to_string(),
            }
        }
    }

    /// Seed a prior FINISHED run's runtime state for an exact
    /// (default-tenant, agent, rig, brief) pairing → a resumable session id.
    /// The run is finished so it is not a `live` run that would block a new
    /// start.
    fn seed_session(
        s: &TaskStore,
        run_id: &str,
        brief: &str,
        agent: &str,
        rig: &str,
        session: &str,
    ) {
        s.record_run_start(
            run_id,
            brief,
            agent,
            rig,
            "manual",
            &crate::nodes::coordinator::RunWorkspaceInfo::default(),
        )
        .unwrap();
        let u = crate::rig::RunUsage {
            session_id: Some(session.to_string()),
            provider: Some("openai".to_string()),
            ..Default::default()
        };
        s.record_run_runtime_state(run_id, &u, "done", None, None)
            .unwrap();
        s.record_run_finish(run_id, "done", "seeded").unwrap();
    }

    #[test]
    fn dispatch_resumes_a_stored_compatible_session() {
        let (s, _tmp) = store_ws();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let rig: Arc<dyn Rig> = Arc::new(ResumeCaptureRig { seen: seen.clone() });
        let b = ready_brief(&s, "resume me", "agt_r");
        // A prior run on the SAME (tenant, agent, rig, brief) left a session.
        seed_session(&s, "seed_run", &b, "agt_r", "capture", "sess-keep");

        let records = dispatch_batch_with_policy(
            &s,
            50,
            300,
            None,
            |_| true,
            |_| 20,
            |_| BudgetAdmission::Allow,
            |_: &brief::BriefCard| Some(rig.clone()),
            |c: &brief::BriefCard| c.title.clone(),
            |_: &brief::BriefCard| RunModelPrefs::default(),
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        let got = seen.lock().unwrap().clone().expect("the rig ran");
        assert_eq!(
            got.as_deref(),
            Some("sess-keep"),
            "the compatible same-scope session resumed into the request"
        );
    }

    #[test]
    fn dispatch_ignores_incompatible_stored_sessions() {
        let (s, _tmp) = store_ws();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let rig: Arc<dyn Rig> = Arc::new(ResumeCaptureRig { seen: seen.clone() });
        let b = ready_brief(&s, "target", "agt_r");
        // Sessions stored under a DIFFERENT Operative or a DIFFERENT Rig — the
        // run for (agt_r, capture, b) must NOT pick either up.
        seed_session(
            &s,
            "seed_other_agent",
            &b,
            "agt_other",
            "capture",
            "sess-agent",
        );
        seed_session(&s, "seed_other_rig", &b, "agt_r", "claude", "sess-rig");

        let records = dispatch_batch_with_policy(
            &s,
            50,
            300,
            None,
            |_| true,
            |_| 20,
            |_| BudgetAdmission::Allow,
            |_: &brief::BriefCard| Some(rig.clone()),
            |c: &brief::BriefCard| c.title.clone(),
            |_: &brief::BriefCard| RunModelPrefs::default(),
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        let got = seen.lock().unwrap().clone().expect("the rig ran");
        assert_eq!(
            got, None,
            "no incompatible session crosses agent or rig scope"
        );
    }

    // ── Allowance / budget hard-stop (company-model §3.6/§5.2D) ──

    #[test]
    fn allowance_admits_pure_verdicts() {
        // No per-agent Allowance configured → always allowed.
        assert_eq!(allowance_admits(None, 999_999_999), BudgetAdmission::Allow);
        // Explicit zero (or negative) Allowance → hard-stop regardless
        // of spend (even with zero recorded spend).
        assert!(matches!(
            allowance_admits(Some(0), 0),
            BudgetAdmission::Refuse { .. }
        ));
        assert!(matches!(
            allowance_admits(Some(-5), 0),
            BudgetAdmission::Refuse { .. }
        ));
        // Positive cap: 100 cents = 1_000_000 micro-USD.
        // Under the cap → allowed.
        assert_eq!(allowance_admits(Some(100), 999_999), BudgetAdmission::Allow);
        // At/over the cap → refused.
        assert!(matches!(
            allowance_admits(Some(100), 1_000_000),
            BudgetAdmission::Refuse { .. }
        ));
        assert!(matches!(
            allowance_admits(Some(100), 5_000_000),
            BudgetAdmission::Refuse { .. }
        ));
    }

    #[test]
    fn over_budget_operative_is_refused_parked_and_chronicled() {
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let reg = RigRegistry::with_builtins();
        let refused = ready_brief(&s, "refused work", "agt_broke");
        let allowed = ready_brief(&s, "allowed work", "agt_ok");

        let records = dispatch_batch_with_policy(
            &s,
            50,
            300,
            None,
            |_| true,
            |_| 20,
            // Refuse only the over-budget Operative; mirror the live
            // closure's payload shape.
            |card: &brief::BriefCard| {
                if card.assignee_agent_id.as_deref() == Some("agt_broke") {
                    BudgetAdmission::Refuse {
                        reason: "budget_refused: agent_id=agt_broke allowance=0c used=0u \
                                 reason=allowance=0 (hard-stopped)"
                            .to_string(),
                        event: OPERATIVE_BUDGET_EVENT,
                        status: OPERATIVE_BUDGET_STATUS,
                    }
                } else {
                    BudgetAdmission::Allow
                }
            },
            |_: &brief::BriefCard| reg.get("echo"),
            |c: &brief::BriefCard| c.title.clone(),
            |_: &brief::BriefCard| RunModelPrefs::default(),
        )
        .unwrap();

        // The refused Brief did NOT run: it's parked in `blocked`
        // (visible to the operator), never reaching `in_review`.
        assert_eq!(
            s.board_status(&refused).unwrap().as_deref(),
            Some("blocked")
        );
        // It was NOT silently skipped — a chronicle event explains why.
        let refusal = s
            .query_events(
                &refused,
                0,
                50,
                Some("brief.budget_refused"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert_eq!(refusal.len(), 1);
        assert!(
            refusal[0].payload.contains("budget_refused")
                && refusal[0].payload.contains("agt_broke"),
            "got {:?}",
            refusal[0].payload
        );
        // The Claim lease is released (not leaked) and the wakeup
        // closed as failed.
        assert!(s.claim_holder(&refused).unwrap().is_none());
        assert!(records.iter().any(|r| r.brief_id == refused
            && matches!(
                r.outcome,
                RigOutcome::Failed {
                    retryable: false,
                    ..
                }
            )));

        // The under-budget Operative still dispatches normally.
        assert_eq!(
            s.board_status(&allowed).unwrap().as_deref(),
            Some("in_review")
        );
    }

    // ── Guild-level budget hard-stop (autonomous) — company-model §6.6 ──

    #[test]
    fn guild_allowance_admits_pure_verdicts() {
        // No Guild budget set → no cap → allow (mirrors the Action Center,
        // which only surfaces a Guild signal for a positive budget). A `0`/
        // negative budget is "unset" here (NOT a company-wide hard-stop — that
        // deliberately differs from the per-Operative `0` = hard-stop).
        assert_eq!(
            guild_allowance_admits(None, 999_999_999),
            BudgetAdmission::Allow
        );
        assert_eq!(guild_allowance_admits(Some(0), 0), BudgetAdmission::Allow);
        assert_eq!(
            guild_allowance_admits(Some(-5), 999_999),
            BudgetAdmission::Allow
        );
        // Positive budget: 100 cents = 1_000_000 micro-USD. Under → allow.
        assert_eq!(
            guild_allowance_admits(Some(100), 999_999),
            BudgetAdmission::Allow
        );
        // At/over the budget → refuse, tagged as the Guild stop.
        match guild_allowance_admits(Some(100), 1_000_000) {
            BudgetAdmission::Refuse { event, status, .. } => {
                assert_eq!(event, GUILD_BUDGET_EVENT);
                assert_eq!(status, GUILD_BUDGET_STATUS);
            }
            other => panic!("expected Guild refusal, got {other:?}"),
        }
        assert!(matches!(
            guild_allowance_admits(Some(100), 5_000_000),
            BudgetAdmission::Refuse { .. }
        ));
    }

    #[test]
    fn allowance_window_is_the_current_utc_calendar_month() {
        // Concrete, well-known UTC midnights (seconds since epoch):
        //   2021-01-01 = 1_609_459_200, 2021-02-01 = 1_612_137_600.
        const JAN_2021: i64 = 1_609_459_200_000;
        const FEB_2021: i64 = 1_612_137_600_000;

        // Any instant inside January 2021 → the whole of January is the window.
        let mid_jan = JAN_2021 + 10 * MS_PER_DAY + 12 * 3_600_000; // Jan 11 12:00Z
        let w = allowance_window(mid_jan);
        assert_eq!(
            w.start_ms, JAN_2021,
            "window opens at the month's first instant"
        );
        assert_eq!(w.cutoff_ms, mid_jan, "cutoff is the supplied now");
        assert_eq!(
            w.resets_at_ms, FEB_2021,
            "reset edge is next month's first instant"
        );
        assert_eq!(w.start_ms % MS_PER_DAY, 0, "start aligns to a UTC midnight");

        // The first instant of the month is INCLUSIVE (matches `cost_since`'s
        // `timestamp_ms >= since`): a window computed exactly at the boundary
        // still opens at that boundary, not the previous month.
        assert_eq!(allowance_window(JAN_2021).start_ms, JAN_2021);

        // Reset bookkeeping: feeding the reset edge yields a fresh window that
        // opens exactly at that edge — month-to-date spend resets by
        // construction (a new window summed from the new start).
        assert_eq!(allowance_window(FEB_2021).start_ms, FEB_2021);
        // 1ms before the boundary still belongs to the previous month, which
        // resets precisely at the boundary.
        assert_eq!(allowance_window(JAN_2021 - 1).resets_at_ms, JAN_2021);

        // Leap February (2024): the window is 29 days long.
        const FEB_2024: i64 = 1_706_745_600_000; // 2024-02-01
        let leap = allowance_window(FEB_2024 + 14 * MS_PER_DAY); // Feb 15 2024
        assert_eq!(leap.start_ms, FEB_2024);
        assert_eq!(
            (leap.resets_at_ms - leap.start_ms) / MS_PER_DAY,
            29,
            "Feb 2024 is a leap month"
        );

        // December → January rollover crosses the year boundary correctly.
        const DEC_2023: i64 = 1_701_388_800_000; // 2023-12-01
        const JAN_2024: i64 = 1_704_067_200_000; // 2024-01-01
        let dec = allowance_window(DEC_2023 + 14 * MS_PER_DAY); // Dec 15 2023
        assert_eq!(dec.start_ms, DEC_2023);
        assert_eq!(
            dec.resets_at_ms, JAN_2024,
            "December resets into the next January"
        );
    }

    /// A priced AI invocation row attributed to `agent_id` in `tenant` — the
    /// dispatch gate / Action Center count cost by the Operative's `agent_id`
    /// (`agent_name` must equal `agent_id`) within the calendar-month window.
    fn guild_spend_row(
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

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// A Brief assigned to `assignee` in Guild `tenant`, ready to run.
    fn ready_brief_in_tenant(s: &TaskStore, title: &str, assignee: &str, tenant: &str) -> String {
        let id = ready_brief(s, title, assignee);
        s.set_task_tenant(&id, tenant).unwrap();
        id
    }

    #[test]
    fn dispatch_budget_admits_refuses_over_guild_budget_but_allows_under_it() {
        use crate::nodes::coordinator::agent::store::AgentStore;
        use crate::nodes::coordinator::spine::SpineStore;
        let s = store();
        let agents = AgentStore::in_memory().unwrap();
        let spine = SpineStore::in_memory().unwrap();

        // One active Operative in guild-a with NO per-Operative Allowance (so the
        // per-Operative gate allows — the Guild gate is the decider).
        let eng = agents
            .ensure_starter_operative("engineer", "Eng A", "Operative", "echo", "guild-a")
            .unwrap()
            .0;
        let brief = ready_brief_in_tenant(&s, "work", &eng, "guild-a");
        let card = s.brief_card(&brief).unwrap().unwrap();

        // Guild budget $200; the Guild has spent $250 → over. Seed at the
        // canonical window start so the row is unconditionally in-month
        // (deterministic at any clock, including the first second of a month).
        spine.set_guild_allowance("guild-a", Some(20_000)).unwrap();
        let in_window = allowance_window(now_ms()).start_ms;
        let mstore = crate::metrics::MetricsStore::in_memory().unwrap();
        mstore
            .insert_batch(&[guild_spend_row(&eng, "guild-a", in_window, 250_000_000)])
            .unwrap();
        let mq = crate::metrics::MetricsQuery::new(mstore);

        match dispatch_budget_admits(&card, &s, &agents, Some(&spine), Some(&mq), now_ms()) {
            BudgetAdmission::Refuse {
                event,
                status,
                reason,
            } => {
                assert_eq!(event, GUILD_BUDGET_EVENT, "tagged as the Guild stop");
                assert_eq!(status, GUILD_BUDGET_STATUS);
                assert!(reason.contains("guild_budget_refused"), "reason: {reason}");
                assert!(reason.contains("guild-a"), "names the Guild: {reason}");
            }
            other => panic!("expected an over-Guild-budget refusal, got {other:?}"),
        }

        // Raise the budget above spend → the SAME autonomous dispatch is allowed.
        spine
            .set_guild_allowance("guild-a", Some(1_000_000))
            .unwrap();
        assert_eq!(
            dispatch_budget_admits(&card, &s, &agents, Some(&spine), Some(&mq), now_ms()),
            BudgetAdmission::Allow,
            "under the Guild budget, autonomous dispatch runs as before"
        );

        // No Guild budget set at all → no cap → allow.
        spine.set_guild_allowance("guild-a", None).unwrap();
        assert_eq!(
            dispatch_budget_admits(&card, &s, &agents, Some(&spine), Some(&mq), now_ms()),
            BudgetAdmission::Allow,
        );
    }

    #[test]
    fn dispatch_budget_admits_per_operative_stop_takes_precedence_over_guild() {
        use crate::nodes::coordinator::agent::store::AgentStore;
        use crate::nodes::coordinator::spine::SpineStore;
        let s = store();
        let agents = AgentStore::in_memory().unwrap();
        let spine = SpineStore::in_memory().unwrap();

        let eng = agents
            .ensure_starter_operative("engineer", "Eng A", "Operative", "echo", "guild-a")
            .unwrap()
            .0;
        // Per-Operative Allowance $1 (100c) — the Operative is itself over.
        agents.update_agent_field(&eng, "allowance", "100").unwrap();
        let brief = ready_brief_in_tenant(&s, "work", &eng, "guild-a");
        let card = s.brief_card(&brief).unwrap().unwrap();

        // BOTH the Operative AND the Guild are over budget.
        spine.set_guild_allowance("guild-a", Some(20_000)).unwrap();
        let in_window = allowance_window(now_ms()).start_ms;
        let mstore = crate::metrics::MetricsStore::in_memory().unwrap();
        mstore
            .insert_batch(&[guild_spend_row(&eng, "guild-a", in_window, 250_000_000)])
            .unwrap();
        let mq = crate::metrics::MetricsQuery::new(mstore);

        // The per-Operative stop wins (authoritative, never weakened): the
        // refusal is tagged `brief.budget_refused` / `over_allowance`, not the
        // Guild stop.
        match dispatch_budget_admits(&card, &s, &agents, Some(&spine), Some(&mq), now_ms()) {
            BudgetAdmission::Refuse { event, status, .. } => {
                assert_eq!(event, OPERATIVE_BUDGET_EVENT);
                assert_eq!(status, OPERATIVE_BUDGET_STATUS);
            }
            other => panic!("expected a per-Operative refusal, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_budget_admits_is_tenant_isolated() {
        use crate::nodes::coordinator::agent::store::AgentStore;
        use crate::nodes::coordinator::spine::SpineStore;
        let s = store();
        let agents = AgentStore::in_memory().unwrap();
        let spine = SpineStore::in_memory().unwrap();

        // guild-a: an Operative that has blown the SHARED metrics ledger.
        let eng_a = agents
            .ensure_starter_operative("engineer", "Eng A", "Operative", "echo", "guild-a")
            .unwrap()
            .0;
        // guild-b: its own Operative with NO spend.
        let eng_b = agents
            .ensure_starter_operative("engineer", "Eng B", "Operative", "echo", "guild-b")
            .unwrap()
            .0;
        let brief_b = ready_brief_in_tenant(&s, "work-b", &eng_b, "guild-b");
        let card_b = s.brief_card(&brief_b).unwrap().unwrap();

        // BOTH Guilds have a $200 budget; only guild-a has spent ($250).
        spine.set_guild_allowance("guild-a", Some(20_000)).unwrap();
        spine.set_guild_allowance("guild-b", Some(20_000)).unwrap();
        let in_window = allowance_window(now_ms()).start_ms;
        let mstore = crate::metrics::MetricsStore::in_memory().unwrap();
        mstore
            .insert_batch(&[guild_spend_row(&eng_a, "guild-a", in_window, 250_000_000)])
            .unwrap();
        let mq = crate::metrics::MetricsQuery::new(mstore);

        // guild-b's Brief is NOT tripped by guild-a's spend — the Guild sum is
        // computed over guild-b's OWN active Operatives only (never cross-tenant).
        assert_eq!(
            dispatch_budget_admits(&card_b, &s, &agents, Some(&spine), Some(&mq), now_ms()),
            BudgetAdmission::Allow,
            "another Guild's spend must not trip this Guild's cap"
        );

        // Sanity: guild-a's own Brief IS refused (proves the spend is real, not
        // simply invisible everywhere).
        let brief_a = ready_brief_in_tenant(&s, "work-a", &eng_a, "guild-a");
        let card_a = s.brief_card(&brief_a).unwrap().unwrap();
        assert!(matches!(
            dispatch_budget_admits(&card_a, &s, &agents, Some(&spine), Some(&mq), now_ms()),
            BudgetAdmission::Refuse { .. }
        ));
    }

    #[test]
    fn dispatch_budget_admits_guild_gate_is_inert_without_metrics_or_spine() {
        use crate::nodes::coordinator::agent::store::AgentStore;
        use crate::nodes::coordinator::spine::SpineStore;
        let s = store();
        let agents = AgentStore::in_memory().unwrap();
        let spine = SpineStore::in_memory().unwrap();

        // An active Operative with NO per-Operative Allowance, in a Guild whose
        // budget is set and WOULD be over — if spend could actually be read.
        let eng = agents
            .ensure_starter_operative("engineer", "Eng A", "Operative", "echo", "guild-a")
            .unwrap()
            .0;
        let brief = ready_brief_in_tenant(&s, "work", &eng, "guild-a");
        let card = s.brief_card(&brief).unwrap().unwrap();
        spine.set_guild_allowance("guild-a", Some(20_000)).unwrap();

        // (a) Metrics ledger unavailable → the Guild gate cannot read spend, so
        // it stays INERT and never fabricates an over-budget refusal. This pins
        // the no-fake-spend contract: a transient/absent metrics ledger must NOT
        // become a phantom Guild hard-stop.
        assert_eq!(
            dispatch_budget_admits(&card, &s, &agents, Some(&spine), None, now_ms()),
            BudgetAdmission::Allow,
            "no metrics ledger → Guild gate inert, never a fabricated over-budget stop"
        );

        // (b) Spine store unavailable → the Guild budget can't be resolved → the
        // gate is inert even with real over-budget spend in the ledger.
        let in_window = allowance_window(now_ms()).start_ms;
        let mstore = crate::metrics::MetricsStore::in_memory().unwrap();
        mstore
            .insert_batch(&[guild_spend_row(&eng, "guild-a", in_window, 250_000_000)])
            .unwrap();
        let mq = crate::metrics::MetricsQuery::new(mstore);
        assert_eq!(
            dispatch_budget_admits(&card, &s, &agents, None, Some(&mq), now_ms()),
            BudgetAdmission::Allow,
            "no spine store → Guild budget unresolved → gate inert (per-Operative gate still applies)"
        );
    }

    #[test]
    fn over_guild_budget_brief_is_parked_and_chronicled_as_guild_stop() {
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let reg = RigRegistry::with_builtins();
        let refused = ready_brief(&s, "guild over-budget work", "agt_guild");

        // The dispatch path records WHATEVER the budget gate reports — here a
        // Guild stop — so run history / the Action Center read the right cause.
        let records = dispatch_batch_with_policy(
            &s,
            50,
            300,
            None,
            |_| true,
            |_| 20,
            |card: &brief::BriefCard| {
                if card.assignee_agent_id.as_deref() == Some("agt_guild") {
                    BudgetAdmission::Refuse {
                        reason: "guild_budget_refused: tenant=default budget=100c guild_used=999u \
                                 reason=over Guild budget"
                            .to_string(),
                        event: GUILD_BUDGET_EVENT,
                        status: GUILD_BUDGET_STATUS,
                    }
                } else {
                    BudgetAdmission::Allow
                }
            },
            |_: &brief::BriefCard| reg.get("echo"),
            |c: &brief::BriefCard| c.title.clone(),
            |_: &brief::BriefCard| RunModelPrefs::default(),
        )
        .unwrap();

        // Parked in `blocked`, never ran.
        assert_eq!(
            s.board_status(&refused).unwrap().as_deref(),
            Some("blocked")
        );
        // Chronicled under the GUILD event type (not the per-Operative one).
        let guild_ev = s
            .query_events(
                &refused,
                0,
                50,
                Some(GUILD_BUDGET_EVENT),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert_eq!(guild_ev.len(), 1, "exactly one guild.budget_refused event");
        assert!(guild_ev[0].payload.contains("guild_budget_refused"));
        // The per-Operative event was NOT used for this stop.
        let op_ev = s
            .query_events(
                &refused,
                0,
                50,
                Some(OPERATIVE_BUDGET_EVENT),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert!(op_ev.is_empty(), "a Guild stop is not a per-Operative stop");
        // Claim released; the record is a non-retryable failure.
        assert!(s.claim_holder(&refused).unwrap().is_none());
        assert!(records.iter().any(|r| r.brief_id == refused
            && matches!(
                r.outcome,
                RigOutcome::Failed {
                    retryable: false,
                    ..
                }
            )));
    }

    #[test]
    fn manual_run_is_sovereign_over_the_guild_budget_gate() {
        use crate::nodes::coordinator::agent::store::AgentStore;
        use crate::nodes::coordinator::spine::SpineStore;
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let agents = AgentStore::in_memory().unwrap();
        let spine = SpineStore::in_memory().unwrap();
        let reg = RigRegistry::with_builtins();

        let eng = agents
            .ensure_starter_operative("engineer", "Eng A", "Operative", "echo", "guild-a")
            .unwrap()
            .0;
        let brief = ready_brief_in_tenant(&s, "work", &eng, "guild-a");
        let card = s.brief_card(&brief).unwrap().unwrap();

        // Guild is over budget → the autonomous gate refuses.
        spine.set_guild_allowance("guild-a", Some(20_000)).unwrap();
        let mstore = crate::metrics::MetricsStore::in_memory().unwrap();
        mstore
            .insert_batch(&[guild_spend_row(
                &eng,
                "guild-a",
                now_ms() - 1_000,
                250_000_000,
            )])
            .unwrap();
        let mq = crate::metrics::MetricsQuery::new(mstore);
        assert!(
            matches!(
                dispatch_budget_admits(&card, &s, &agents, Some(&spine), Some(&mq), now_ms()),
                BudgetAdmission::Refuse { .. }
            ),
            "autonomous dispatch is refused over the Guild budget"
        );

        // The manual operator path (`preflight_run`) takes NO budget gate — the
        // Board is sovereign — so the SAME over-budget Brief commits to a run.
        match preflight_run(&s, &reg, None, 300, &brief, Some("echo"), "go".to_string()).unwrap() {
            Preflight::Ready(_) => {}
            Preflight::Refused(r) => panic!(
                "manual run must stay sovereign over the Guild cap, got refusal: {}",
                r.status
            ),
        }
    }

    #[test]
    fn dispatch_batch_fails_a_brief_with_no_rig_and_leaves_the_board() {
        let s = store();
        let a = ready_brief(&s, "x", "agt_a"); // todo
        let records = dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| None,
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0].outcome, RigOutcome::Failed { .. }));
        // Nothing ran → board untouched (still todo); Claim released.
        assert_eq!(s.board_status(&a).unwrap().as_deref(), Some("todo"));
        assert!(s.claim_holder(&a).unwrap().is_none());
        // …but the refusal is durable: a `refused` Shift (no_adapter, heartbeat)
        // so the operator can later see WHY the autonomous run never happened.
        let runs = s.runs_for_brief(&a, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "refused");
        assert_eq!(runs[0].refusal_reason.as_deref(), Some("no_adapter"));
        assert_eq!(runs[0].trigger.as_deref(), Some("heartbeat"));
    }

    #[test]
    fn dispatch_batch_parks_an_unrecoverable_failure_in_blocked() {
        // A Rig that always fails non-retryably.
        struct DeadRig;
        impl Rig for DeadRig {
            fn name(&self) -> &str {
                "dead"
            }
            fn run(&self, _req: &RigRunRequest) -> RigOutcome {
                RigOutcome::Failed {
                    reason: "boom".to_string(),
                    retryable: false,
                }
            }
        }
        let (s, _tmp) = store_ws();
        let a = ready_brief(&s, "x", "agt_a"); // todo
        let rig: Arc<dyn Rig> = Arc::new(DeadRig);

        let records = dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| Some(rig.clone()),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();
        assert_eq!(records.len(), 1);
        assert!(matches!(
            records[0].outcome,
            RigOutcome::Failed {
                retryable: false,
                ..
            }
        ));
        // Started then failed unrecoverably → parked in blocked,
        // Claim released, and no longer ready (won't re-dispatch).
        assert_eq!(s.board_status(&a).unwrap().as_deref(), Some("blocked"));
        assert!(s.claim_holder(&a).unwrap().is_none());
        assert!(s.list_ready_briefs(50).unwrap().is_empty());
        // The reason is chronicled so the Desk can show why.
        let events = s
            .query_events(
                &a,
                0,
                50,
                Some("brief.dispatch_failed"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        // Unified pipeline chronicles the adapter-tagged reason.
        assert_eq!(events[0].payload, "[dead] boom");
    }

    #[test]
    fn dispatch_batch_leaves_a_retryable_failure_in_progress() {
        struct FlakyRig;
        impl Rig for FlakyRig {
            fn name(&self) -> &str {
                "flaky"
            }
            fn run(&self, _req: &RigRunRequest) -> RigOutcome {
                RigOutcome::Failed {
                    reason: "transient".to_string(),
                    retryable: true,
                }
            }
        }
        let (s, _tmp) = store_ws();
        let a = ready_brief(&s, "x", "agt_a"); // todo
        let rig: Arc<dyn Rig> = Arc::new(FlakyRig);
        dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| Some(rig.clone()),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();
        // Retryable → stays in_progress so the next tick retries it.
        assert_eq!(s.board_status(&a).unwrap().as_deref(), Some("in_progress"));
    }

    #[test]
    fn dispatch_batch_chronicles_a_continue_note_and_stays_in_progress() {
        struct YieldRig;
        impl Rig for YieldRig {
            fn name(&self) -> &str {
                "yield"
            }
            fn run(&self, _req: &RigRunRequest) -> RigOutcome {
                RigOutcome::Continue {
                    note: "waiting on review".to_string(),
                }
            }
        }
        let (s, _tmp) = store_ws();
        let a = ready_brief(&s, "a", "agt_a"); // todo
        let rig: Arc<dyn Rig> = Arc::new(YieldRig);
        dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| Some(rig.clone()),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();

        // Stays in_progress (resumable) and the note is chronicled.
        assert_eq!(s.board_status(&a).unwrap().as_deref(), Some("in_progress"));
        let events = s
            .query_events(
                &a,
                0,
                50,
                Some("brief.continued"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert_eq!(events.len(), 1);
        // Unified pipeline chronicles the adapter-tagged note.
        assert_eq!(events[0].payload, "[yield] waiting on review");
    }

    #[test]
    fn dispatch_batch_hands_the_rig_the_brief_spine_context() {
        use std::sync::{Arc, Mutex};

        struct CtxRig(Arc<Mutex<String>>);
        impl Rig for CtxRig {
            fn name(&self) -> &str {
                "ctx"
            }
            fn run(&self, req: &RigRunRequest) -> RigOutcome {
                *self.0.lock().unwrap() = req.context.clone();
                RigOutcome::Done {
                    summary: "ok".to_string(),
                }
            }
        }

        let (s, _tmp) = store_ws();
        let a = ready_brief(&s, "a", "agt_a");
        s.set_brief_field(&a, "priority", "high").unwrap();
        s.set_brief_field(&a, "mandate", "mandate_x").unwrap();
        s.set_brief_field(&a, "campaign", "camp_y").unwrap();

        let seen = Arc::new(Mutex::new(String::new()));
        let rig: Arc<dyn Rig> = Arc::new(CtxRig(seen.clone()));
        dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| Some(rig.clone()),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();

        let ctx = seen.lock().unwrap().clone();
        assert!(ctx.contains("priority=high"), "ctx: {ctx}");
        assert!(ctx.contains("mandate=mandate_x"), "ctx: {ctx}");
        assert!(ctx.contains("campaign=camp_y"), "ctx: {ctx}");
    }

    #[test]
    fn dispatch_batch_mints_and_revokes_a_bridge_token_per_run() {
        use std::sync::{Arc, Mutex};

        // A Rig that records the bridge token it was handed.
        struct RecordingRig {
            token: Arc<Mutex<String>>,
            allowed: Arc<Mutex<bool>>,
            denied: Arc<Mutex<bool>>,
            tokens: BridgeTokenStore,
        }
        impl Rig for RecordingRig {
            fn name(&self) -> &str {
                "recorder"
            }
            fn run(&self, req: &RigRunRequest) -> RigOutcome {
                *self.token.lock().unwrap() = req.bridge_token.clone();
                *self.allowed.lock().unwrap() = self.tokens.authorize_method(
                    &req.bridge_token,
                    &req.brief_id,
                    &req.agent_id,
                    "brief.comment",
                );
                *self.denied.lock().unwrap() = !self.tokens.authorize_method(
                    &req.bridge_token,
                    &req.brief_id,
                    &req.agent_id,
                    "agent.delete",
                );
                RigOutcome::Done {
                    summary: "ok".to_string(),
                }
            }
        }

        let (s, _tmp) = store_ws();
        let _a = ready_brief(&s, "a", "agt_a");
        let tokens = BridgeTokenStore::new();
        let seen = Arc::new(Mutex::new(String::new()));
        let allowed = Arc::new(Mutex::new(false));
        let denied = Arc::new(Mutex::new(false));
        let rig: Arc<dyn Rig> = Arc::new(RecordingRig {
            token: seen.clone(),
            allowed: allowed.clone(),
            denied: denied.clone(),
            tokens: tokens.clone(),
        });

        let records = dispatch_batch(
            &s,
            50,
            300,
            Some(&tokens),
            |_: &brief::BriefCard| Some(rig.clone()),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();
        assert_eq!(records.len(), 1);

        // A token was minted and handed to the Rig during the run…
        let handed = seen.lock().unwrap().clone();
        assert!(handed.starts_with("brt_"), "got: {handed:?}");
        assert!(
            *allowed.lock().unwrap(),
            "token must permit run-scoped bridge-back methods"
        );
        assert!(
            *denied.lock().unwrap(),
            "token must deny unrelated bridge methods"
        );
        // …and revoked when the Shift ended.
        assert!(tokens.is_empty(), "token should be revoked after the run");
    }

    // ── Autonomous/manual unification: one ledger, one pipeline ──
    //
    // An autonomous heartbeat run must produce the SAME durable output as a
    // manual dashboard run — a `brief_runs` row, a transcript, artifacts,
    // and review state — distinguished only by the `heartbeat` trigger.

    /// One autonomous heartbeat tick over the ready Briefs with a given Rig
    /// — the test analog of the live timer dispatch (`dispatch_batch`).
    fn heartbeat_tick(
        s: &TaskStore,
        rig: Option<Arc<dyn Rig>>,
        tokens: Option<&BridgeTokenStore>,
    ) -> Vec<DispatchRecord> {
        dispatch_batch(
            s,
            50,
            300,
            tokens,
            move |_: &brief::BriefCard| rig.clone(),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap()
    }

    #[test]
    fn heartbeat_run_produces_a_ledger_row_transcript_and_review_state() {
        let (s, _tmp) = store_ws();
        let reg = crate::rig::RigRegistry::with_builtins();
        let a = ready_brief(&s, "auto docs", "agt_a");

        let records = heartbeat_tick(&s, reg.get("echo"), None);
        assert_eq!(records.len(), 1);

        // A durable run row exists in the SAME ledger the dashboard polls.
        let runs = s.list_runs(50).unwrap();
        let run = runs
            .iter()
            .find(|r| r.brief_id == a)
            .expect("autonomous run must open a brief_runs row");
        assert_eq!(run.status, "done");
        assert_eq!(run.trigger.as_deref(), Some("heartbeat"));
        // A `done` autonomous run enters review, exactly like a manual one.
        assert_eq!(run.review.as_deref(), Some("pending_review"));

        // Transcript events were recorded for the autonomous run.
        let kinds: Vec<String> = s
            .list_run_events(&run.run_id, 200)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&"accepted".to_string()));
        assert!(kinds.contains(&"process_started".to_string()));
        assert!(kinds.contains(&"result".to_string()));
    }

    #[test]
    fn heartbeat_run_records_artifacts_for_a_file_writing_rig() {
        let (s, _tmp) = store_ws();
        let reg = file_creating_rig("auto_note.txt", "hi");
        let a = ready_brief(&s, "auto file", "agt_a");

        heartbeat_tick(&s, reg.get("mk"), None);
        let run = s
            .list_runs(50)
            .unwrap()
            .into_iter()
            .find(|r| r.brief_id == a)
            .expect("autonomous run row");
        assert_eq!(run.trigger.as_deref(), Some("heartbeat"));
        let arts = s.list_run_artifacts(&run.run_id).unwrap();
        assert!(
            arts.iter()
                .any(|x| x.rel_path == "auto_note.txt" && x.kind == "created"),
            "heartbeat run must record the file it created: {arts:?}"
        );
    }

    #[test]
    fn run_trigger_is_manual_for_dashboard_and_heartbeat_for_timer() {
        let (s, _tmp) = store_ws();
        // Manual run via the dashboard "Start" path.
        let m = ready_brief(&s, "manual one", "agt_m");
        let report = run_brief_now(
            &s,
            &echo_registry(),
            None,
            300,
            &m,
            Some("echo"),
            "x".into(),
        )
        .unwrap();
        let mrun = s.get_run(&report.run_id.unwrap()).unwrap().unwrap();
        assert_eq!(mrun.trigger.as_deref(), Some("manual"));

        // Autonomous run via a heartbeat tick.
        let reg = crate::rig::RigRegistry::with_builtins();
        let h = ready_brief(&s, "auto one", "agt_h");
        heartbeat_tick(&s, reg.get("echo"), None);
        let hrun = s
            .list_runs(50)
            .unwrap()
            .into_iter()
            .find(|r| r.brief_id == h)
            .unwrap();
        assert_eq!(hrun.trigger.as_deref(), Some("heartbeat"));
    }

    #[test]
    fn heartbeat_failed_run_records_a_failed_run_row_and_event() {
        struct DeadRig;
        impl Rig for DeadRig {
            fn name(&self) -> &str {
                "dead"
            }
            fn run(&self, _req: &RigRunRequest) -> RigOutcome {
                RigOutcome::Failed {
                    reason: "boom".to_string(),
                    retryable: false,
                }
            }
        }
        let (s, _tmp) = store_ws();
        let a = ready_brief(&s, "auto fail", "agt_a");
        let rig: Arc<dyn Rig> = Arc::new(DeadRig);

        heartbeat_tick(&s, Some(rig), None);
        let run = s
            .list_runs(50)
            .unwrap()
            .into_iter()
            .find(|r| r.brief_id == a)
            .expect("a failed autonomous execution still opens a run row");
        assert_eq!(run.status, "failed");
        assert_eq!(run.trigger.as_deref(), Some("heartbeat"));
        // Non-retryable → parked in blocked (not re-dispatched forever).
        assert_eq!(s.board_status(&a).unwrap().as_deref(), Some("blocked"));
        let kinds: Vec<String> = s
            .list_run_events(&run.run_id, 200)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&"failed".to_string()));
    }

    #[test]
    fn heartbeat_adapter_unavailable_refuses_with_durable_refused_run() {
        struct DownRig;
        impl Rig for DownRig {
            fn name(&self) -> &str {
                "down"
            }
            fn probe(&self) -> crate::rig::RigProbe {
                crate::rig::RigProbe::missing("not installed", Some("install it".to_string()))
            }
            fn run(&self, _req: &RigRunRequest) -> RigOutcome {
                panic!("must NOT spawn an unavailable adapter");
            }
        }
        let (s, _tmp) = store_ws();
        let a = ready_brief(&s, "auto down", "agt_a");
        let rig: Arc<dyn Rig> = Arc::new(DownRig);

        let records = heartbeat_tick(&s, Some(rig), None);
        // Refused at the readiness gate: the adapter is NEVER spawned (the
        // `run` panic above proves it), but the refusal is now a durable
        // `refused` Shift so the operator can later see WHY it didn't run.
        let rows: Vec<_> = s
            .list_runs(50)
            .unwrap()
            .into_iter()
            .filter(|r| r.brief_id == a)
            .collect();
        assert_eq!(rows.len(), 1, "exactly one (refused) row");
        assert_eq!(rows[0].status, "refused");
        assert_eq!(
            rows[0].refusal_reason.as_deref(),
            Some("adapter_unavailable")
        );
        assert_eq!(rows[0].trigger.as_deref(), Some("heartbeat"));
        assert_eq!(rows[0].rig, "down");
        // No executed run exists (refused is not an execution).
        assert!(
            !s.list_runs(50)
                .unwrap()
                .iter()
                .any(|r| r.brief_id == a && r.status == "running"),
            "an unavailable adapter must not open a running run row"
        );
        assert!(records.iter().any(|r| r.brief_id == a
            && matches!(
                r.outcome,
                RigOutcome::Failed {
                    retryable: false,
                    ..
                }
            )));
        // The Claim is released, so the Brief can retry once the adapter is up.
        assert!(s.claim_holder(&a).unwrap().is_none());
    }

    #[test]
    fn heartbeat_run_does_not_auto_apply() {
        let (s, _tmp) = store_ws();
        let reg = file_creating_rig("auto_note.txt", "hi");
        let a = ready_brief(&s, "auto file", "agt_a");

        heartbeat_tick(&s, reg.get("mk"), None);
        let run = s
            .list_runs(50)
            .unwrap()
            .into_iter()
            .find(|r| r.brief_id == a)
            .unwrap();
        // Autonomous runs are review-gated exactly like manual runs — no
        // apply happens automatically.
        assert!(
            run.apply_status.is_none(),
            "autonomous run must NOT auto-apply"
        );
        assert_eq!(run.review.as_deref(), Some("pending_review"));
        assert!(
            run_apply_eligibility(&run).is_err(),
            "an unreviewed autonomous run is not apply-eligible"
        );
    }

    #[test]
    fn a_live_claim_prevents_a_duplicate_autonomous_run() {
        let (s, _tmp) = store_ws();
        let reg = crate::rig::RigRegistry::with_builtins();
        let a = ready_brief(&s, "contended", "agt_a");
        // Another worker already holds a live Claim on the Brief.
        assert!(
            s.claim_brief_for_run(&a, "other_agent", 300, Some("other_run"))
                .unwrap()
        );

        // A heartbeat tick must NOT start a second concurrent run on it.
        let records = heartbeat_tick(&s, reg.get("echo"), None);
        assert!(
            records.iter().all(|r| r.brief_id != a),
            "a live-claimed Brief must not double-run"
        );
        assert!(
            s.list_runs(50).unwrap().is_empty(),
            "the heartbeat opened no run row for the claimed Brief"
        );
    }

    #[test]
    fn heartbeat_run_row_is_tenant_scoped() {
        let (s, _tmp) = store_ws();
        let reg = crate::rig::RigRegistry::with_builtins();
        let a = ready_brief(&s, "auto scoped", "agt_a");
        s.set_task_tenant(&a, "guild-a").unwrap();

        heartbeat_tick(&s, reg.get("echo"), None);
        let run = s
            .list_runs(50)
            .unwrap()
            .into_iter()
            .find(|r| r.brief_id == a)
            .unwrap();
        assert!(s.run_belongs_to_tenant(&run.run_id, "guild-a").unwrap());
        assert!(
            !s.run_belongs_to_tenant(&run.run_id, "guild-b").unwrap(),
            "another Guild cannot see the autonomous run"
        );
    }

    // ── Synchronous run_brief_now (the dashboard "Start") ────────

    fn echo_registry() -> crate::rig::RigRegistry {
        crate::rig::RigRegistry::with_builtins().with_default("echo")
    }

    #[test]
    fn run_brief_now_runs_on_echo_and_moves_to_review() {
        let (s, _tmp) = store_ws();
        let id = ready_brief(&s, "Write the readme", "agt_a");
        let reg = echo_registry();
        let report =
            run_brief_now(&s, &reg, None, 300, &id, Some("echo"), "do the work".into()).unwrap();
        assert_eq!(report.status, "done", "got: {report:?}");
        assert_eq!(report.rig, "echo");
        assert!(report.summary.contains("echo:"));
        // The board advanced to review and the run was chronicled.
        assert_eq!(s.board_status(&id).unwrap().as_deref(), Some("in_review"));
        let kinds: Vec<String> = s
            .list_events_after(&id, 0, 100)
            .unwrap()
            .into_iter()
            .map(|e| e.event_type)
            .collect();
        assert!(kinds.iter().any(|k| k == "brief.run_started"));
        assert!(kinds.iter().any(|k| k == "brief.shift_done"));
        // The Claim is released after the run.
        assert!(s.claim_holder(&id).unwrap().is_none());
    }

    #[test]
    fn run_brief_now_refuses_unassigned() {
        let s = store();
        let id = s
            .create("u", "f", "{}", "subj", RetryPolicy::None, 0, None, None)
            .unwrap();
        let report = run_brief_now(
            &s,
            &echo_registry(),
            None,
            300,
            &id,
            Some("echo"),
            "x".into(),
        )
        .unwrap();
        assert_eq!(report.status, "unassigned");
    }

    #[test]
    fn run_brief_now_reports_no_adapter_when_none_resolves() {
        let s = store();
        let id = ready_brief(&s, "t", "agt_a");
        let empty = crate::rig::RigRegistry::new(); // no default, no rigs
        let report = run_brief_now(&s, &empty, None, 300, &id, None, "x".into()).unwrap();
        assert_eq!(report.status, "no_adapter");
    }

    #[test]
    fn run_brief_now_reports_adapter_unavailable_without_spawning() {
        let s = store();
        let id = ready_brief(&s, "t", "agt_a");
        let mut reg = crate::rig::RigRegistry::new();
        reg.register(std::sync::Arc::new(
            crate::rig::ProcessRig::new(
                "ghost",
                "definitely-not-installed-relix-adapter-xyzzy",
                vec![],
            )
            .with_install_hint("install the ghost adapter"),
        ));
        reg.set_default(Some("ghost".to_string()));
        let report = run_brief_now(&s, &reg, None, 300, &id, None, "x".into()).unwrap();
        assert_eq!(report.status, "adapter_unavailable", "got: {report:?}");
        assert_eq!(report.rig, "ghost");
        assert_eq!(
            report.install_hint.as_deref(),
            Some("install the ghost adapter")
        );
        // It must NOT have moved the board (no run happened).
        assert_eq!(s.board_status(&id).unwrap().as_deref(), Some("todo"));
    }

    #[test]
    fn run_brief_now_reports_not_found_for_unknown_brief() {
        let s = store();
        let report = run_brief_now(
            &s,
            &echo_registry(),
            None,
            300,
            "nope",
            Some("echo"),
            "x".into(),
        )
        .unwrap();
        assert_eq!(report.status, "not_found");
    }

    #[test]
    fn run_brief_now_opens_and_closes_a_durable_run_record() {
        let (s, _tmp) = store_ws();
        let reg = crate::rig::RigRegistry::with_builtins();
        let id = ready_brief(&s, "ship it", "agt_a");

        let report =
            run_brief_now(&s, &reg, None, 300, &id, Some("echo"), "do the work".into()).unwrap();
        assert_eq!(report.status, "done");
        let run_id = report.run_id.clone().expect("a committed run has a run_id");

        // The run is in the durable ledger, terminal, with a duration and
        // the adapter that ran it — no event-string parsing needed.
        let runs = s.runs_for_brief(&id, 10).unwrap();
        assert_eq!(runs.len(), 1, "exactly one run recorded");
        let r = &runs[0];
        assert_eq!(r.run_id, run_id);
        assert_eq!(r.brief_id, id);
        assert_eq!(r.agent_id, "agt_a");
        assert_eq!(r.rig, "echo");
        assert_eq!(r.status, "done");
        assert!(r.finished_at.is_some(), "a finished run has finished_at");
        assert!(r.duration_secs.is_some());
        assert!(r.summary.contains("echo:"), "got {:?}", r.summary);

        // It also shows up in the recent-runs feed.
        let recent = s.list_runs(50).unwrap();
        assert!(recent.iter().any(|x| x.run_id == run_id));
    }

    #[test]
    fn preflight_refusal_records_no_run() {
        // An unavailable adapter must never leave a phantom run row.
        let s = store();
        let id = ready_brief(&s, "t", "agt_a");
        let mut reg = crate::rig::RigRegistry::new();
        reg.register(std::sync::Arc::new(crate::rig::ProcessRig::new(
            "ghost",
            "definitely-not-installed-relix-adapter-xyzzy",
            vec![],
        )));
        reg.set_default(Some("ghost".to_string()));

        let report = run_brief_now(&s, &reg, None, 300, &id, None, "x".into()).unwrap();
        assert_eq!(report.status, "adapter_unavailable");
        assert!(report.run_id.is_none(), "a refusal carries no run_id");
        assert!(
            s.runs_for_brief(&id, 10).unwrap().is_empty(),
            "no run record for a pre-flight refusal"
        );
        assert!(s.list_runs(50).unwrap().is_empty());
    }

    #[test]
    fn preflight_then_execute_matches_the_synchronous_path() {
        // The async split (preflight → execute_ready) yields the same
        // durable run + board outcome as run_brief_now.
        let (s, _tmp) = store_ws();
        let reg = crate::rig::RigRegistry::with_builtins();
        let id = ready_brief(&s, "async shift", "agt_a");

        let pre = preflight_run(&s, &reg, None, 300, &id, Some("echo"), "work".into()).unwrap();
        let ready = match pre {
            Preflight::Ready(r) => *r,
            Preflight::Refused(r) => panic!("expected ready, got {r:?}"),
        };
        // The run is already recorded as `running` before execution.
        let opened = s.runs_for_brief(&id, 10).unwrap();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].status, "running");
        assert!(opened[0].finished_at.is_none());

        let report = execute_ready(&s, None, ready);
        assert_eq!(report.status, "done");
        let closed = s.runs_for_brief(&id, 10).unwrap();
        assert_eq!(closed[0].status, "done");
        assert!(closed[0].finished_at.is_some());
        assert_eq!(s.board_status(&id).unwrap().as_deref(), Some("in_review"));
    }

    // ── Scoped per-run workspaces ───────────────────────────────

    fn empty_cfg() -> WorkspaceConfig {
        WorkspaceConfig::default() // context = Empty
    }

    /// Build a tiny temp "project" with a few files + dangerous dirs/files
    /// that `copy_repo` must exclude. Returns the project root tempdir.
    fn fake_project() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        std::fs::write(p.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(p.join("README.md"), "hello").unwrap();
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::write(p.join("src").join("lib.rs"), "pub fn x() {}").unwrap();
        // Dangerous / excluded entries:
        std::fs::create_dir_all(p.join(".git")).unwrap();
        std::fs::write(p.join(".git").join("HEAD"), "ref: x").unwrap();
        std::fs::create_dir_all(p.join("target")).unwrap();
        std::fs::write(p.join("target").join("huge.bin"), vec![0u8; 1024]).unwrap();
        std::fs::create_dir_all(p.join(".claude").join("worktrees")).unwrap();
        std::fs::write(p.join(".claude").join("worktrees").join("stale"), "x").unwrap();
        std::fs::create_dir_all(p.join("node_modules")).unwrap();
        std::fs::write(p.join("node_modules").join("dep.js"), "x").unwrap();
        std::fs::create_dir_all(p.join("dev-data")).unwrap();
        std::fs::write(p.join("dev-data").join("tasks.db"), "x").unwrap();
        std::fs::write(p.join(".env"), "SECRET=1").unwrap();
        std::fs::write(p.join("api.key"), "abc").unwrap();
        tmp
    }

    fn copy_repo_cfg(root: &std::path::Path) -> WorkspaceConfig {
        WorkspaceConfig {
            context: WorkspaceContext::CopyRepo,
            project_root: root.to_path_buf(),
            max_bytes: DEFAULT_WORKSPACE_MAX_BYTES,
            max_files: DEFAULT_WORKSPACE_MAX_FILES,
        }
    }

    #[test]
    fn empty_mode_creates_only_brief_md() {
        let tmp = tempfile::tempdir().unwrap();
        let prepared = prepare_run_workspace(
            &tmp.path().join("runs"),
            "run_abc123",
            "brief_1",
            "Ship it",
            "priority=normal",
            &empty_cfg(),
        )
        .unwrap();
        assert!(prepared.path.is_dir(), "workspace dir created");
        assert!(prepared.path.ends_with("run_abc123"));
        assert_eq!(prepared.context, WorkspaceContext::Empty);
        assert_eq!(prepared.copied_files, 0);
        // Only BRIEF.md present.
        let names: Vec<String> = std::fs::read_dir(&prepared.path)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["BRIEF.md".to_string()]);
        let brief_md = std::fs::read_to_string(prepared.path.join("BRIEF.md")).unwrap();
        assert!(brief_md.contains("brief_1") && brief_md.contains("Ship it"));
        assert!(brief_md.contains("workspace_context: empty"));
    }

    #[test]
    fn copy_repo_copies_normal_files_and_excludes_dangerous() {
        let proj = fake_project();
        let dst_root = tempfile::tempdir().unwrap();
        let prepared = prepare_run_workspace(
            &dst_root.path().join("runs"),
            "run_copy1",
            "b",
            "t",
            "c",
            &copy_repo_cfg(proj.path()),
        )
        .unwrap();
        assert_eq!(prepared.context, WorkspaceContext::CopyRepo);
        let ws = &prepared.path;
        // Normal files copied (+ BRIEF.md).
        assert!(ws.join("main.rs").is_file());
        assert!(ws.join("README.md").is_file());
        assert!(ws.join("src").join("lib.rs").is_file());
        assert!(ws.join("BRIEF.md").is_file());
        // Dangerous / excluded entries NOT copied.
        assert!(!ws.join(".git").exists(), ".git must be excluded");
        assert!(!ws.join("target").exists(), "target must be excluded");
        assert!(
            !ws.join(".claude").join("worktrees").exists(),
            ".claude/worktrees must be excluded"
        );
        assert!(!ws.join("node_modules").exists(), "node_modules excluded");
        assert!(!ws.join("dev-data").exists(), "dev-data excluded");
        assert!(!ws.join(".env").exists(), ".env (secret) excluded");
        assert!(!ws.join("api.key").exists(), "*.key (secret) excluded");
        // Stats reflect ONLY the copied project files (3: main.rs, README.md, src/lib.rs).
        assert_eq!(prepared.copied_files, 3, "only the 3 safe files");
        assert!(prepared.copied_bytes > 0);
    }

    #[test]
    fn copy_repo_refuses_when_file_cap_exceeded() {
        let proj = fake_project();
        let dst_root = tempfile::tempdir().unwrap();
        let mut cfg = copy_repo_cfg(proj.path());
        cfg.max_files = 1; // the project has 3 copyable files
        let err = prepare_run_workspace(
            &dst_root.path().join("runs"),
            "run_cap1",
            "b",
            "t",
            "c",
            &cfg,
        )
        .unwrap_err();
        assert_eq!(err.status(), "workspace_context_error");
        assert!(
            err.message().contains("file-count cap"),
            "got: {}",
            err.message()
        );
        // The partial workspace was cleaned up (no half-copied tree).
        assert!(!dst_root.path().join("runs").join("run_cap1").exists());
    }

    #[test]
    fn copy_repo_refuses_when_byte_cap_exceeded() {
        let proj = fake_project();
        let dst_root = tempfile::tempdir().unwrap();
        let mut cfg = copy_repo_cfg(proj.path());
        cfg.max_bytes = 4; // smaller than any real file
        let err = prepare_run_workspace(
            &dst_root.path().join("runs"),
            "run_cap2",
            "b",
            "t",
            "c",
            &cfg,
        )
        .unwrap_err();
        assert_eq!(err.status(), "workspace_context_error");
        assert!(err.message().contains("size cap"), "got: {}", err.message());
    }

    #[test]
    fn copy_repo_rejects_unsafe_project_root() {
        let dst_root = tempfile::tempdir().unwrap();
        let mut cfg = copy_repo_cfg(std::path::Path::new("/definitely/not/a/real/dir/xyzzy"));
        cfg.context = WorkspaceContext::CopyRepo;
        let err = prepare_run_workspace(
            &dst_root.path().join("runs"),
            "run_badroot",
            "b",
            "t",
            "c",
            &cfg,
        )
        .unwrap_err();
        assert_eq!(err.status(), "workspace_context_error");
        assert!(
            err.message().contains("project root"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn workspace_rejects_path_traversal_run_ids() {
        assert!(run_id_is_safe("run_1f75a50e-ee53-4771-8d01-294fde5b623d"));
        for bad in [
            "..",
            ".",
            "a/b",
            "a\\b",
            "../escape",
            "run_/..",
            "",
            "a b",
            "x/../y",
        ] {
            assert!(!run_id_is_safe(bad), "{bad:?} must be rejected");
        }
        // And prepare refuses an unsafe id (never touches the filesystem
        // outside its single validated segment).
        let tmp = tempfile::tempdir().unwrap();
        let err = prepare_run_workspace(tmp.path(), "../escape", "b", "t", "c", &empty_cfg())
            .unwrap_err();
        assert_eq!(err.status(), "workspace_error");
        assert!(
            err.message().contains("unsafe run id"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn run_executes_inside_its_scoped_workspace() {
        // A ProcessRig that prints its working directory — its cwd must be
        // the per-run workspace (not the coordinator CWD).
        let (s, _tmp) = store_ws();
        let id = ready_brief(&s, "where am I", "agt_a");
        let (prog, args) = if cfg!(windows) {
            ("cmd".to_string(), vec!["/C".to_string(), "cd".to_string()])
        } else {
            ("sh".to_string(), vec!["-c".to_string(), "pwd".to_string()])
        };
        let mut reg = crate::rig::RigRegistry::new();
        reg.register(std::sync::Arc::new(crate::rig::ProcessRig::new(
            "pwd", prog, args,
        )));
        reg.set_default(Some("pwd".to_string()));

        let report = run_brief_now(&s, &reg, None, 300, &id, None, "x".into()).unwrap();
        assert_eq!(report.status, "done", "got: {report:?}");
        let run_id = report.run_id.clone().unwrap();
        // The run ledger persists the workspace path (NOT secret-redacted)
        // and it is the per-run scoped dir.
        let runs = s.runs_for_brief(&id, 5).unwrap();
        let ws = runs[0].workspace.as_deref().expect("workspace recorded");
        assert!(
            ws.ends_with(&run_id),
            "ledger workspace {ws} ends with {run_id}"
        );
        assert_eq!(report.workspace.as_deref(), Some(ws));
        // The child's printed cwd is UNDER that scoped workspace root. (The
        // run_id segment itself is redacted in the summary because it is a
        // 40-char base64url-shaped token — expected; the ledger keeps the
        // real path.) The parent (`…/runs`) is enough to prove the cwd.
        let ws_parent = std::path::Path::new(ws)
            .parent()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            report.summary.contains(&ws_parent),
            "cwd {:?} should sit under the workspace root {ws_parent}",
            report.summary
        );
    }

    #[test]
    fn copy_repo_run_executes_with_copied_context_as_cwd() {
        // End-to-end: a run with copy_repo context lists its cwd and sees
        // the copied project files + BRIEF.md; the ledger records the mode
        // + copied stats.
        let proj = fake_project();
        let (mut s, _tmp) = store_ws();
        s.set_run_workspace_config(copy_repo_cfg(proj.path()));
        let id = ready_brief(&s, "ls my workspace", "agt_a");
        let (prog, args) = if cfg!(windows) {
            (
                "cmd".to_string(),
                vec!["/C".to_string(), "dir".to_string(), "/b".to_string()],
            )
        } else {
            ("sh".to_string(), vec!["-c".to_string(), "ls".to_string()])
        };
        let mut reg = crate::rig::RigRegistry::new();
        reg.register(std::sync::Arc::new(crate::rig::ProcessRig::new(
            "ls", prog, args,
        )));
        reg.set_default(Some("ls".to_string()));

        let report = run_brief_now(&s, &reg, None, 300, &id, None, "x".into()).unwrap();
        assert_eq!(report.status, "done", "got: {report:?}");
        // The cwd listing shows the copied files (proving copy + cwd).
        assert!(
            report.summary.contains("main.rs"),
            "ls {:?}",
            report.summary
        );
        assert!(
            report.summary.contains("BRIEF.md"),
            "ls {:?}",
            report.summary
        );
        assert!(
            !report.summary.contains(".git"),
            "excluded dirs not present"
        );
        // Ledger + report carry the context mode + copy stats.
        assert_eq!(report.workspace_context.as_deref(), Some("copy_repo"));
        assert_eq!(report.workspace_files, Some(3));
        let runs = s.runs_for_brief(&id, 5).unwrap();
        assert_eq!(runs[0].workspace_context.as_deref(), Some("copy_repo"));
        assert_eq!(runs[0].workspace_files, Some(3));
        assert!(runs[0].workspace_bytes.unwrap() > 0);
    }

    #[test]
    fn workspace_creation_failure_refuses_cleanly() {
        // Root pointed at an existing FILE → create_dir_all fails →
        // a clean `workspace_error` refusal: no run record, Claim released,
        // no repo-wide fallback.
        let tmp = tempfile::tempdir().unwrap();
        let file_root = tmp.path().join("not-a-dir");
        std::fs::write(&file_root, b"x").unwrap();
        let mut s = TaskStore::in_memory().unwrap();
        s.set_run_workspace_root(&file_root);
        let id = ready_brief(&s, "t", "agt_a");
        let reg = echo_registry();

        let report = run_brief_now(&s, &reg, None, 300, &id, Some("echo"), "x".into()).unwrap();
        assert_eq!(report.status, "workspace_error", "got: {report:?}");
        assert!(report.run_id.is_none());
        assert!(report.summary.contains("workspace"));
        // No phantom run row; Claim released so a fixed config can retry.
        assert!(s.runs_for_brief(&id, 5).unwrap().is_empty());
        assert!(s.claim_holder(&id).unwrap().is_none());
    }

    #[test]
    fn concurrent_runs_get_distinct_non_colliding_workspaces() {
        // execution-and-issue-design §2.6: concurrency comes from having MANY
        // Briefs (one run per issue), not from racing one Brief. The same
        // Operative running two DIFFERENT Briefs at once gets distinct,
        // non-colliding run ids + workspaces (the unique run id is the
        // workspace key). (Two starts on the SAME Brief is the duplicate-start
        // case — refused `already_running`, see
        // `duplicate_same_operative_start_refused_already_running`.)
        let (s, _tmp) = store_ws();
        let reg = crate::rig::RigRegistry::with_builtins();
        let id1 = ready_brief(&s, "dup-a", "agt_a");
        let id2 = ready_brief(&s, "dup-b", "agt_a");
        let first = preflight_run(&s, &reg, None, 300, &id1, Some("echo"), "x".into()).unwrap();
        let r1 = match first {
            Preflight::Ready(r) => *r,
            Preflight::Refused(r) => panic!("expected ready, got {r:?}"),
        };
        let second = preflight_run(&s, &reg, None, 300, &id2, Some("echo"), "y".into()).unwrap();
        let r2 = match second {
            Preflight::Ready(r) => *r,
            Preflight::Refused(r) => panic!("expected ready, got {r:?}"),
        };
        assert_ne!(r1.run_id, r2.run_id, "distinct run ids");
        let w1 = r1.workspace.clone().unwrap();
        let w2 = r2.workspace.clone().unwrap();
        assert_ne!(w1, w2, "distinct workspace dirs — no collision");
        assert!(std::path::Path::new(&w1).is_dir());
        assert!(std::path::Path::new(&w2).is_dir());
        // One committed run record per Brief, each with its workspace recorded.
        assert_eq!(s.runs_for_brief(&id1, 5).unwrap().len(), 1);
        assert_eq!(s.runs_for_brief(&id2, 5).unwrap().len(), 1);
    }

    #[test]
    fn duplicate_same_operative_start_refused_already_running() {
        // execution-and-issue-design §1.4 (idempotent self-ownership) / §2.6
        // (one run per issue): two manual/Prime starts for the SAME Brief by
        // the SAME assigned Operative must NOT both start. The lower-level
        // `claim_brief_for_run` deliberately lets the same Operative refresh a
        // live Claim (heartbeat/lease idempotency) and the start path mints a
        // NEW run id each time, so the start-path guard is what prevents a
        // duplicate run row/workspace. The first start is Ready+running; the
        // second is refused `already_running` (→ HTTP 409) and opens NOTHING.
        let (s, _tmp) = store_ws();
        let reg = echo_registry();
        let id = ready_brief(&s, "no double start", "agt_a");

        let first = preflight_run(&s, &reg, None, 300, &id, Some("echo"), "x".into()).unwrap();
        let r1 = match first {
            Preflight::Ready(r) => *r,
            Preflight::Refused(r) => panic!("expected the first start to be ready, got {r:?}"),
        };
        // The first start is durably `running` (the live-run evidence the guard
        // keys on) and holds the Claim for its assignee.
        let opened = s.runs_for_brief(&id, 10).unwrap();
        assert_eq!(opened.len(), 1, "exactly one run row after the first start");
        assert_eq!(opened[0].status, "running");
        assert_eq!(s.claim_holder(&id).unwrap().unwrap().0, "agt_a");
        let w1 = r1.workspace.clone().unwrap();

        // Second start by the SAME Operative on the SAME Brief → conflict.
        let second = preflight_run(&s, &reg, None, 300, &id, Some("echo"), "y".into()).unwrap();
        let report = match second {
            Preflight::Refused(r) => r,
            Preflight::Ready(_) => panic!("the duplicate start must be refused, not ready"),
        };
        assert_eq!(report.status, "already_running", "got: {report:?}");
        assert!(report.run_id.is_none(), "a conflict opens no run row");
        assert!(report.workspace.is_none(), "a conflict opens no workspace");
        // No SECOND run row / workspace was opened; the first run still owns it.
        let after = s.runs_for_brief(&id, 10).unwrap();
        assert_eq!(after.len(), 1, "no duplicate run row");
        assert_eq!(after[0].run_id, r1.run_id);
        assert_eq!(s.claim_holder(&id).unwrap().unwrap().0, "agt_a");

        // Once the first run FINISHES (claim released, run no longer running),
        // a fresh start IS allowed — the guard never blocks legitimate
        // continuation, only an overlapping duplicate.
        let _ = execute_ready(&s, None, r1);
        assert!(s.live_run_by_agent(&id, "agt_a").unwrap().is_none());
        let third = preflight_run(&s, &reg, None, 300, &id, Some("echo"), "z".into()).unwrap();
        let r3 = match third {
            Preflight::Ready(r) => *r,
            Preflight::Refused(r) => panic!("a post-completion start must be allowed, got {r:?}"),
        };
        assert_ne!(
            r3.workspace.as_deref(),
            Some(w1.as_str()),
            "fresh workspace"
        );
        assert_eq!(s.runs_for_brief(&id, 10).unwrap().len(), 2);
    }

    #[test]
    fn concurrent_same_operative_starts_one_wins_one_conflicts() {
        // execution-and-issue-design §1.4/§2.6 + the per-Operative start lock:
        // two starts race the SAME Brief with the SAME assigned Operative. The
        // start lock serializes them; EXACTLY ONE wins (Ready+running) and the
        // loser is refused `already_running` (→ HTTP 409). The loser must NEVER
        // retry a 409 — a retry while the winner is live loses again.
        let (s, _tmp) = store_ws();
        let s = std::sync::Arc::new(s);
        let reg = std::sync::Arc::new(echo_registry());
        let id = ready_brief(&s, "self race", "agt_a");

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for prompt in ["a", "b"] {
            let s = s.clone();
            let reg = reg.clone();
            let id = id.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                preflight_run(&s, &reg, None, 300, &id, Some("echo"), prompt.into()).unwrap()
            }));
        }
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let ready = outcomes
            .iter()
            .filter(|o| matches!(o, Preflight::Ready(_)))
            .count();
        let conflicts = outcomes
            .iter()
            .filter(|o| matches!(o, Preflight::Refused(r) if r.status == "already_running"))
            .count();
        assert_eq!(ready, 1, "exactly one start wins");
        assert_eq!(conflicts, 1, "the other start conflicts");
        // Exactly one run row / workspace was opened, despite the race.
        let runs = s.runs_for_brief(&id, 10).unwrap();
        assert_eq!(runs.len(), 1, "no duplicate run row from the race");
        assert_eq!(runs[0].status, "running");

        // NEVER retry a 409: another start while the winner is live conflicts again.
        let retry = preflight_run(&s, &reg, None, 300, &id, Some("echo"), "retry".into()).unwrap();
        assert!(
            matches!(retry, Preflight::Refused(r) if r.status == "already_running"),
            "a retry of a 409 conflict loses again — clients must not retry"
        );
        assert_eq!(
            s.runs_for_brief(&id, 10).unwrap().len(),
            1,
            "still one run row"
        );
    }

    #[test]
    fn preflight_run_refuses_already_running_when_another_run_holds_the_claim() {
        // execution-and-issue-design §1.4/§7.1 (LOCKED two-pointer Claim): the
        // manual start path refuses with `already_running` when a DIFFERENT
        // active execution already owns the Brief's Claim. This is the refusal
        // the HTTP bridge maps to 409 Conflict (never a retryable 200).
        let (s, _tmp) = store_ws();
        let reg = echo_registry();
        let id = ready_brief(&s, "contended manual start", "agt_a");
        // A different worker already holds a live Claim on the Brief.
        assert!(
            s.claim_brief_for_run(&id, "other_agent", 300, Some("other_run"))
                .unwrap()
        );

        let report =
            match preflight_run(&s, &reg, None, 300, &id, Some("echo"), "x".into()).unwrap() {
                Preflight::Refused(r) => r,
                Preflight::Ready(_) => panic!("expected a conflict refusal, got a ready run"),
            };
        assert_eq!(report.status, "already_running", "got: {report:?}");
        assert!(report.run_id.is_none(), "a conflict opens no run row");
        // The other worker's Claim is untouched — the loser never stole it.
        assert_eq!(s.claim_holder(&id).unwrap().unwrap().0, "other_agent");
        // No run row was opened for the refused start.
        assert!(s.runs_for_brief(&id, 5).unwrap().is_empty());
    }

    #[test]
    fn preflight_run_adopts_a_stale_claim_with_terminal_run_evidence() {
        // execution-and-issue-design §1.4 "stale-run adoption" / §7.1 LOCKED
        // two-pointer Claim: a prior Shift left a LIVE Claim (held by a now-dead
        // worker) pointing at a run that already reached a terminal state. A new
        // start must NOT be stuck on `already_running` until the lease ages out
        // — terminal evidence proves the prior Shift ended, so the Claim is
        // reclaimed and the start succeeds.
        let (s, _tmp) = store_ws();
        let reg = echo_registry();
        let id = ready_brief(&s, "stale terminal claim", "agt_a");
        // A previous owner holds a LIVE Claim pointing at run_prev...
        assert!(
            s.claim_brief_for_run(&id, "agt_prev", 300, Some("run_prev"))
                .unwrap()
        );
        // ...but run_prev has already finished terminal (`done`).
        s.record_run_start(
            "run_prev",
            &id,
            "agt_prev",
            "echo",
            "manual",
            &crate::nodes::coordinator::RunWorkspaceInfo::default(),
        )
        .unwrap();
        s.record_run_finish("run_prev", "done", "ok").unwrap();

        let ready =
            match preflight_run(&s, &reg, None, 300, &id, Some("echo"), "go".into()).unwrap() {
                Preflight::Ready(r) => *r,
                Preflight::Refused(r) => {
                    panic!("terminal evidence should let the start proceed, got {r:?}")
                }
            };
        // The new start owns the Brief now; exactly one fresh `running` row.
        assert_eq!(s.claim_holder(&id).unwrap().unwrap().0, "agt_a");
        let runs = s.runs_for_brief(&id, 10).unwrap();
        assert_eq!(
            runs.iter().filter(|r| r.status == "running").count(),
            1,
            "exactly one live run after adoption"
        );
        assert!(runs.iter().any(|r| r.run_id == ready.run_id));
        // A reclaim Chronicle note records the adoption honestly.
        assert_eq!(
            s.query_events(
                &id,
                0,
                50,
                Some("brief.claim_reclaimed"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap()
            .len(),
            1
        );
        let _ = execute_ready(&s, None, ready);
    }

    #[test]
    fn preflight_run_refuses_when_other_workers_run_is_still_running() {
        // The adoption path must NEVER steal a Claim that still backs a RUNNING
        // run: a different worker holds a live Claim whose execution run is
        // actually `running` → the new start is refused `already_running`
        // (→ HTTP 409), exactly as before. NEVER retry a 409.
        let (s, _tmp) = store_ws();
        let reg = echo_registry();
        let id = ready_brief(&s, "live other run", "agt_a");
        assert!(
            s.claim_brief_for_run(&id, "agt_prev", 300, Some("run_prev"))
                .unwrap()
        );
        s.record_run_start(
            "run_prev",
            &id,
            "agt_prev",
            "echo",
            "manual",
            &crate::nodes::coordinator::RunWorkspaceInfo::default(),
        )
        .unwrap(); // still `running` — a live owner

        let report =
            match preflight_run(&s, &reg, None, 300, &id, Some("echo"), "x".into()).unwrap() {
                Preflight::Refused(r) => r,
                Preflight::Ready(_) => panic!("a still-running owner must block the start"),
            };
        assert_eq!(report.status, "already_running", "got: {report:?}");
        assert!(report.run_id.is_none(), "a conflict opens no run row");
        assert_eq!(
            s.claim_holder(&id).unwrap().unwrap().0,
            "agt_prev",
            "the live owner's Claim is untouched"
        );
        // No SECOND run row was opened; only the owner's `running` row exists.
        assert_eq!(
            s.runs_for_brief(&id, 10).unwrap().len(),
            1,
            "no duplicate run row"
        );
        // NEVER retry a 409: a retry while the owner runs loses again.
        let retry = preflight_run(&s, &reg, None, 300, &id, Some("echo"), "y".into()).unwrap();
        assert!(
            matches!(retry, Preflight::Refused(r) if r.status == "already_running"),
            "a retry of a 409 conflict loses again — clients must not retry"
        );
    }

    // ── Autonomous heartbeat: stale-claim adoption by terminal evidence ──
    // (execution-and-issue-design §1.4 "stale-run adoption" / §7.1 LOCKED
    // two-pointer Claim) — the heartbeat shares the SAME `reclaim_terminal_claim`
    // helper as the manual/Prime start, so a dangling live Claim on a terminal
    // run no longer waits for the age-based recovery sweep.

    /// Seed a Brief whose LIVE Claim (held by `holder`) dangles on a `run_id`
    /// that has already finished in the durable ledger with terminal `status`.
    fn dangling_terminal_claim(s: &TaskStore, id: &str, holder: &str, run_id: &str, status: &str) {
        assert!(
            s.claim_brief_for_run(id, holder, 300, Some(run_id))
                .unwrap()
        );
        s.record_run_start(
            run_id,
            id,
            holder,
            "echo",
            "heartbeat",
            &crate::nodes::coordinator::RunWorkspaceInfo::default(),
        )
        .unwrap();
        s.record_run_finish(run_id, status, "prior shift ended")
            .unwrap();
    }

    fn reclaim_chronicle_count(s: &TaskStore, id: &str) -> usize {
        s.query_events(
            id,
            0,
            50,
            Some("brief.claim_reclaimed"),
            crate::nodes::coordinator::EventOrder::Desc,
        )
        .unwrap()
        .len()
    }

    #[test]
    fn heartbeat_adopts_a_terminal_stale_claim_then_dispatches() {
        // The remaining edge: a Brief's Claim is still LIVE (lease not yet aged
        // out) but the run it points at already finished terminal — the owner
        // crashed before releasing. Age-based recovery can't help (the run is no
        // longer `running`), and `list_ready_briefs` excludes the live-claimed
        // Brief — so before this slice the heartbeat had to wait for the lease to
        // expire. Now the dispatch tick adopts the stale Claim and runs it.
        let (s, _tmp) = store_ws();
        let reg = echo_registry();
        let id = ready_brief(&s, "stranded brief", "agt_a");
        // The assignee's own prior Shift went terminal but left a live Claim.
        dangling_terminal_claim(&s, &id, "agt_a", "run_prev", "done");
        assert!(
            s.claim_holder(&id).unwrap().is_some(),
            "claim is live before the tick"
        );

        let records = dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| reg.get("echo"),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();

        // The Brief was dispatched this same tick.
        assert_eq!(
            records.len(),
            1,
            "the adopted Brief dispatched: {records:?}"
        );
        assert_eq!(records[0].brief_id, id);
        assert!(matches!(records[0].outcome, RigOutcome::Done { .. }));
        // Exactly one reclaim was chronicled (the adoption), and the board
        // advanced to review with the Claim released after the run.
        assert_eq!(reclaim_chronicle_count(&s, &id), 1, "one reclaim note");
        assert_eq!(s.board_status(&id).unwrap().as_deref(), Some("in_review"));
        assert!(
            s.claim_holder(&id).unwrap().is_none(),
            "claim released after the dispatched run"
        );
        // A fresh terminal run exists beyond the prior one (no duplicate live run).
        let runs = s.runs_for_brief(&id, 10).unwrap();
        assert!(runs.len() >= 2, "the new run plus the prior one: {runs:?}");
        assert_eq!(
            runs.iter().filter(|r| r.status == "running").count(),
            0,
            "no run left running after the tick"
        );
    }

    #[test]
    fn heartbeat_never_reclaims_a_live_running_claim() {
        // SAFETY: a Claim backing a run that is STILL `running` must never be
        // stolen — that is a live owner, not a stale one. The admission step
        // leaves it alone and the heartbeat does not dispatch it.
        let (s, _tmp) = store_ws();
        let reg = echo_registry();
        let id = ready_brief(&s, "live owner", "agt_a");
        assert!(
            s.claim_brief_for_run(&id, "agt_live", 300, Some("run_live"))
                .unwrap()
        );
        s.record_run_start(
            "run_live",
            &id,
            "agt_live",
            "echo",
            "heartbeat",
            &crate::nodes::coordinator::RunWorkspaceInfo::default(),
        )
        .unwrap(); // still `running`

        let reclaimed = s.reclaim_terminal_claims_ready(50).unwrap();
        assert!(reclaimed.is_empty(), "a running run is never reclaimed");

        let records = dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| reg.get("echo"),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();
        assert!(records.is_empty(), "a live-owned Brief is not dispatched");
        assert_eq!(
            s.claim_holder(&id).unwrap().unwrap().0,
            "agt_live",
            "the live owner's Claim is untouched"
        );
        assert_eq!(reclaim_chronicle_count(&s, &id), 0, "no reclaim note");
    }

    #[test]
    fn heartbeat_does_not_reclaim_when_the_pointer_moved_to_a_newer_running_run() {
        // SAFETY (pointer changed before release): an OLD run going terminal must
        // not release a Claim that a NEWER, still-running run already re-acquired.
        // The candidate scan keys on the Claim's CURRENT pointer (the newer run),
        // which is `running` — so the terminal old run is not evidence and the
        // admission step is a no-op. This is the admission-level mirror of the
        // store guard `reclaim_terminal_claim_does_not_clobber_a_newer_running_claim`.
        let s = store();
        let id = ready_brief(&s, "re-claimed before reclaim", "agt_a");
        // run_old finished terminal...
        s.record_run_start(
            "run_old",
            &id,
            "agt_a",
            "echo",
            "heartbeat",
            &crate::nodes::coordinator::RunWorkspaceInfo::default(),
        )
        .unwrap();
        s.record_run_finish("run_old", "interrupted", "gone")
            .unwrap();
        // ...but a newer run re-claimed the Brief and is running; the Claim now
        // points at run_new, not the terminal run_old.
        assert!(
            s.claim_brief_for_run(&id, "agt_a", 300, Some("run_new"))
                .unwrap()
        );
        s.record_run_start(
            "run_new",
            &id,
            "agt_a",
            "echo",
            "heartbeat",
            &crate::nodes::coordinator::RunWorkspaceInfo::default(),
        )
        .unwrap();

        let reclaimed = s.reclaim_terminal_claims_ready(50).unwrap();
        assert!(
            reclaimed.is_empty(),
            "the newer running Claim must not be clobbered by the old terminal run"
        );
        assert_eq!(
            s.claim_holder(&id).unwrap().unwrap().0,
            "agt_a",
            "the newer running Claim survives"
        );
        assert_eq!(reclaim_chronicle_count(&s, &id), 0, "no reclaim note");
    }

    #[test]
    fn heartbeat_terminal_claim_adoption_is_idempotent() {
        // Running the admission twice records AT MOST one reclaim and promotes a
        // pending deferred wake only once — no duplicate reclaim, wakeup, or run.
        let (s, _tmp) = store_ws();
        let id = ready_brief(&s, "adopt once", "agt_a");
        dangling_terminal_claim(&s, &id, "agt_dead", "run_prev", "failed");
        // A wake that arrived while the dead Claim was live is sitting deferred.
        let deferred = s
            .request_brief_wakeup(&id, "agt_a", "assignment", "assigned", None)
            .unwrap();
        assert_eq!(deferred.status, "deferred", "queued behind the live Claim");

        let first = s.reclaim_terminal_claims_ready(50).unwrap();
        assert_eq!(first, vec![id.clone()], "first pass reclaims it");
        // The deferred wake is promoted to queued exactly once by the release.
        let promoted: Vec<String> = s
            .list_brief_wakeups(&id, 50)
            .unwrap()
            .into_iter()
            .filter(|w| w.status == "queued")
            .map(|w| w.wakeup_id)
            .collect();
        assert_eq!(promoted.len(), 1, "exactly one queued wake after promotion");

        let second = s.reclaim_terminal_claims_ready(50).unwrap();
        assert!(second.is_empty(), "second pass finds nothing to reclaim");
        assert_eq!(
            reclaim_chronicle_count(&s, &id),
            1,
            "no duplicate reclaim note"
        );
        // Still exactly one queued wake — the no-op second pass added none.
        let still_queued = s
            .list_brief_wakeups(&id, 50)
            .unwrap()
            .into_iter()
            .filter(|w| w.status == "queued")
            .count();
        assert_eq!(
            still_queued, 1,
            "no duplicate wake from the idempotent re-run"
        );
    }

    #[test]
    fn heartbeat_terminal_claim_adoption_is_tenant_safe() {
        // Each reclaim touches only its own Brief's Claim, so a terminal-stale
        // Claim in one Guild never releases a live Claim in another. Guild A's
        // dangling terminal Claim is adopted; Guild B's live running Claim is not.
        let s = store();
        let a = ready_brief(&s, "guild a stale", "agt_a");
        s.set_task_tenant(&a, "guild_a").unwrap();
        dangling_terminal_claim(&s, &a, "agt_a", "run_a", "done");

        let b = ready_brief(&s, "guild b live", "agt_b");
        s.set_task_tenant(&b, "guild_b").unwrap();
        assert!(
            s.claim_brief_for_run(&b, "agt_b", 300, Some("run_b"))
                .unwrap()
        );
        s.record_run_start(
            "run_b",
            &b,
            "agt_b",
            "echo",
            "heartbeat",
            &crate::nodes::coordinator::RunWorkspaceInfo::default(),
        )
        .unwrap(); // still `running` in Guild B

        let reclaimed = s.reclaim_terminal_claims_ready(50).unwrap();
        assert_eq!(
            reclaimed,
            vec![a.clone()],
            "only Guild A's stale Claim adopted"
        );
        assert!(s.claim_holder(&a).unwrap().is_none(), "Guild A released");
        assert_eq!(
            s.claim_holder(&b).unwrap().unwrap().0,
            "agt_b",
            "Guild B's live Claim untouched"
        );
    }

    /// The Brief Claim's current `execution_run_id` pointer (None when unset).
    fn claim_exec_run_id(s: &TaskStore, id: &str) -> Option<String> {
        let conn = s.conn.lock().unwrap();
        conn.query_row(
            "SELECT execution_run_id FROM tasks WHERE task_id = ?1",
            rusqlite::params![id],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
    }

    #[test]
    fn heartbeat_claim_pointer_is_a_run_ledger_id_not_a_shift_id() {
        // The fix: the autonomous heartbeat claim path mints ONE durable run id at
        // wakeup-queue time and carries it through, so the Claim's `execution_run_id`
        // IS the `brief_runs.run_id` the dispatcher records — not a `shift_?` claim
        // pointer paired with a separately-minted `run_?` ledger row. Without that
        // single id, a heartbeat-origin Claim could never be adopted by terminal
        // evidence (the pointer never matched a recorded run).
        let s = store();
        let id = ready_brief(&s, "aligned ids", "agt_a");
        // Queue the timer wake exactly as the live heartbeat does.
        let dec = s
            .request_brief_wakeup(&id, "agt_a", "timer", "heartbeat", None)
            .unwrap();
        assert_eq!(dec.status, "queued");
        let queued_run = dec.execution_run_id.clone().unwrap();
        assert!(
            queued_run.starts_with("run_"),
            "a queued heartbeat wake mints a run-ledger id, not shift_: {queued_run}"
        );
        // Claim it through the SAME path `dispatch_batch_with_policy` uses.
        let claimed = s.claim_queued_wakeups_with_caps(50, 300, |_| 20).unwrap();
        assert_eq!(claimed.len(), 1, "the queued wake is claimed");
        let claim_run = claimed[0].run_id.clone();
        assert!(
            claim_run.starts_with("run_"),
            "the claim carries a run-ledger id: {claim_run}"
        );
        // The carried id, the Claim pointer on the Brief row, and the queued
        // wake's id are ALL the same id — so the run the dispatcher records under
        // it will match for terminal-evidence adoption.
        assert_eq!(claim_run, queued_run, "one id from queue through claim");
        assert_eq!(
            claim_exec_run_id(&s, &id).as_deref(),
            Some(claim_run.as_str()),
            "the Brief Claim's execution_run_id == the run-ledger id"
        );
        // And it is a safe workspace-path segment (run_<uuid>).
        assert!(run_id_is_safe(&claim_run), "claim run id is workspace-safe");
    }

    #[test]
    fn heartbeat_origin_dangling_terminal_claim_is_adopted_and_redispatched() {
        // The real heartbeat-origin case the caveat flagged: a Brief is claimed
        // through the wakeup queue, its run reaches a TERMINAL state, but the owner
        // dies before releasing the Claim (and before finishing the wake). The
        // Claim is still LIVE, so `list_ready_briefs` excludes it and age-based
        // recovery can't help (the run is no longer `running`). Before the id
        // alignment the Claim pointed at `shift_?` while the ledger row was `run_?`,
        // so terminal evidence never matched and the Brief waited for lease expiry.
        // Now the Claim pointer IS the terminal run's id, so one heartbeat tick
        // adopts it and re-dispatches the SAME tick — with no duplicate run/wake.
        let (s, _tmp) = store_ws();
        let reg = echo_registry();
        let id = ready_brief(&s, "heartbeat origin", "agt_a");
        // Drive the REAL heartbeat claim path (queue → claim).
        let dec = s
            .request_brief_wakeup(&id, "agt_a", "timer", "heartbeat", None)
            .unwrap();
        let claimed = s.claim_queued_wakeups_with_caps(50, 300, |_| 20).unwrap();
        assert_eq!(claimed.len(), 1);
        let run_id = claimed[0].run_id.clone();
        assert_eq!(dec.execution_run_id.as_deref(), Some(run_id.as_str()));
        // The dispatcher recorded a terminal run under THIS id, then the owner died
        // before releasing the Claim / finishing the wake.
        s.record_run_start(
            &run_id,
            &id,
            "agt_a",
            "echo",
            "heartbeat",
            &crate::nodes::coordinator::RunWorkspaceInfo::default(),
        )
        .unwrap();
        s.record_run_finish(&run_id, "done", "prior heartbeat shift ended")
            .unwrap();
        assert!(
            s.claim_holder(&id).unwrap().is_some(),
            "the heartbeat-origin Claim is still live before the tick"
        );

        // One heartbeat tick adopts + re-dispatches.
        let records = dispatch_batch(
            &s,
            50,
            300,
            None,
            |_: &brief::BriefCard| reg.get("echo"),
            |c: &brief::BriefCard| c.title.clone(),
        )
        .unwrap();

        assert_eq!(
            records.len(),
            1,
            "adopted + re-dispatched this tick: {records:?}"
        );
        assert_eq!(records[0].brief_id, id);
        assert!(matches!(records[0].outcome, RigOutcome::Done { .. }));
        assert_eq!(
            reclaim_chronicle_count(&s, &id),
            1,
            "exactly one reclaim note (terminal-evidence adoption)"
        );
        assert_eq!(s.board_status(&id).unwrap().as_deref(), Some("in_review"));
        assert!(
            s.claim_holder(&id).unwrap().is_none(),
            "the Claim is released after the re-dispatched run"
        );
        // A fresh terminal run beyond the adopted one, none left running.
        let runs = s.runs_for_brief(&id, 10).unwrap();
        assert!(
            runs.len() >= 2,
            "the new run plus the prior terminal one: {runs:?}"
        );
        assert_eq!(
            runs.iter().filter(|r| r.status == "running").count(),
            0,
            "no run left running after the tick"
        );
        // No duplicate live wake: the stale heartbeat wake was closed by the
        // reclaim and the re-dispatch's wake finished — none left queued/running.
        let live_wakes = s
            .list_brief_wakeups(&id, 50)
            .unwrap()
            .into_iter()
            .filter(|w| matches!(w.status.as_str(), "queued" | "running"))
            .count();
        assert_eq!(
            live_wakes, 0,
            "no leftover queued/running wake after re-dispatch"
        );
    }

    #[test]
    fn reclaim_closes_only_the_wake_tied_to_the_terminal_run() {
        // SAFETY: the stale-wake cleanup in `reclaim_terminal_claim` matches ONLY
        // the wake whose id == the reclaimed (terminal) run pointer. An unrelated
        // queued wake for a DIFFERENT Brief is never touched.
        let s = store();
        let a = ready_brief(&s, "terminal-stale", "agt_a");
        let b = ready_brief(&s, "unrelated queued", "agt_b");
        // a: a heartbeat-origin Claim dangling on a terminal run (its wake running).
        let dec_a = s
            .request_brief_wakeup(&a, "agt_a", "timer", "heartbeat", None)
            .unwrap();
        let claimed = s.claim_queued_wakeups_with_caps(50, 300, |_| 20).unwrap();
        let run_a = claimed
            .iter()
            .find(|c| c.card.task_id == a)
            .unwrap()
            .run_id
            .clone();
        assert_eq!(dec_a.execution_run_id.as_deref(), Some(run_a.as_str()));
        s.record_run_start(
            &run_a,
            &a,
            "agt_a",
            "echo",
            "heartbeat",
            &crate::nodes::coordinator::RunWorkspaceInfo::default(),
        )
        .unwrap();
        s.record_run_finish(&run_a, "done", "ended").unwrap();
        // b: an independent wake sitting queued the whole time.
        let dec_b = s
            .request_brief_wakeup(&b, "agt_b", "timer", "heartbeat", None)
            .unwrap();
        assert_eq!(dec_b.status, "queued");

        let reclaimed = s.reclaim_terminal_claims_ready(50).unwrap();
        assert_eq!(
            reclaimed,
            vec![a.clone()],
            "only a's terminal-stale Claim adopted"
        );
        // b's queued wake is untouched (different Brief, different run id).
        let b_queued = s
            .list_brief_wakeups(&b, 50)
            .unwrap()
            .into_iter()
            .filter(|w| w.status == "queued")
            .count();
        assert_eq!(
            b_queued, 1,
            "the unrelated queued wake survives the reclaim"
        );
    }

    #[test]
    fn concurrent_starts_one_wins_loser_gets_conflict_and_a_retry_loses_again() {
        // execution-and-issue-design §1.4/§7.1 + §2.6 (per-Operative start
        // lock): two starts race the SAME Brief held by a live external Claim.
        // The start path is serialized per Operative and every contender is
        // refused `already_running` (→ HTTP 409). The rule the loser honors:
        // NEVER retry a 409 — a retry while the holder is live loses again.
        let (s, _tmp) = store_ws();
        let s = std::sync::Arc::new(s);
        let reg = std::sync::Arc::new(echo_registry());
        let id = ready_brief(&s, "race", "agt_a");
        // An external worker holds the live Claim, so BOTH contenders below
        // (which resolve the Brief's assignee, agt_a) must lose deterministically.
        assert!(
            s.claim_brief_for_run(&id, "other_agent", 300, Some("other_run"))
                .unwrap()
        );

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for prompt in ["a", "b"] {
            let s = s.clone();
            let reg = reg.clone();
            let id = id.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                preflight_run(&s, &reg, None, 300, &id, Some("echo"), prompt.into()).unwrap()
            }));
        }
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let conflicts = outcomes
            .iter()
            .filter(|o| matches!(o, Preflight::Refused(r) if r.status == "already_running"))
            .count();
        assert_eq!(
            conflicts, 2,
            "both contenders lose to the live external Claim"
        );
        // No run rows were opened; the external Claim still stands.
        assert!(s.runs_for_brief(&id, 5).unwrap().is_empty());
        assert_eq!(s.claim_holder(&id).unwrap().unwrap().0, "other_agent");

        // NEVER retry a 409: another start while the holder is live conflicts again.
        let retry = preflight_run(&s, &reg, None, 300, &id, Some("echo"), "retry".into()).unwrap();
        assert!(
            matches!(retry, Preflight::Refused(r) if r.status == "already_running"),
            "a retry of a 409 conflict loses again — clients must not retry"
        );
    }

    // ── Run transcript + cancellation ───────────────────────────

    #[test]
    fn run_records_a_lifecycle_transcript() {
        let (s, _tmp) = store_ws();
        let id = ready_brief(&s, "transcript", "agt_a");
        let report = run_brief_now(
            &s,
            &echo_registry(),
            None,
            300,
            &id,
            Some("echo"),
            "work".into(),
        )
        .unwrap();
        let run_id = report.run_id.unwrap();
        let kinds: Vec<String> = s
            .list_run_events(&run_id, 100)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        // The lifecycle is present + chronological (accepted first).
        assert_eq!(kinds.first().map(String::as_str), Some("accepted"));
        for k in [
            "accepted",
            "workspace_prepared",
            "process_started",
            "process_exited",
            "result",
        ] {
            assert!(kinds.contains(&k.to_string()), "missing {k}: {kinds:?}");
        }
        // get_run returns the same run.
        assert_eq!(s.get_run(&run_id).unwrap().unwrap().run_id, run_id);
    }

    #[test]
    fn append_run_event_caps_and_records_truncated() {
        let s = store();
        let cap = crate::nodes::coordinator::MAX_RUN_EVENTS;
        for i in 0..(cap + 5) {
            s.append_run_event("run_t", "tick", "relix", &format!("ev {i}"), None, false)
                .unwrap();
        }
        let events = s.list_run_events("run_t", cap + 50).unwrap();
        // cap real events + exactly one truncation marker.
        assert_eq!(events.len() as i64, cap + 1);
        assert_eq!(events.last().unwrap().kind, "truncated");
        // A second overflow does NOT add another marker.
        s.append_run_event("run_t", "tick", "relix", "more", None, false)
            .unwrap();
        let again = s.list_run_events("run_t", cap + 50).unwrap();
        assert_eq!(again.len() as i64, cap + 1);
    }

    #[test]
    fn cancellation_marks_the_run_cancelled() {
        // A ProcessRig polls the cancel flag; setting it before execution
        // makes the run report `cancelled` (process killed), not `failed`.
        let (s, _tmp) = store_ws();
        let id = ready_brief(&s, "stoppable", "agt_a");
        let (prog, args) = if cfg!(windows) {
            (
                "cmd".to_string(),
                vec!["/C".to_string(), "echo".to_string(), "hi".to_string()],
            )
        } else {
            (
                "sh".to_string(),
                vec!["-c".to_string(), "echo hi".to_string()],
            )
        };
        let mut reg = crate::rig::RigRegistry::new();
        reg.register(std::sync::Arc::new(crate::rig::ProcessRig::new(
            "p", prog, args,
        )));
        reg.set_default(Some("p".to_string()));

        let ready = match preflight_run(&s, &reg, None, 300, &id, None, "x".into()).unwrap() {
            Preflight::Ready(r) => *r,
            Preflight::Refused(r) => panic!("expected ready, got {r:?}"),
        };
        let run_id = ready.run_id.clone();
        // Operator cancels before/while it runs.
        assert!(crate::rig::CancelRegistry::global().request(&run_id));
        let report = execute_ready(&s, None, ready);
        assert_eq!(report.status, "cancelled", "got: {report:?}");
        assert_eq!(s.get_run(&run_id).unwrap().unwrap().status, "cancelled");
        assert!(
            s.list_run_events(&run_id, 100)
                .unwrap()
                .iter()
                .any(|e| e.kind == "cancelled"),
            "a cancelled transcript event is recorded"
        );
    }

    // ── Run artifacts + review ──────────────────────────────────

    /// A ProcessRig that creates a file in its cwd (the run workspace).
    fn file_creating_rig(name: &str, content: &str) -> crate::rig::RigRegistry {
        let (prog, args) = if cfg!(windows) {
            (
                "cmd".to_string(),
                vec!["/C".to_string(), format!("echo {content}> {name}")],
            )
        } else {
            (
                "sh".to_string(),
                vec!["-c".to_string(), format!("printf '{content}' > {name}")],
            )
        };
        let mut reg = crate::rig::RigRegistry::new();
        reg.register(std::sync::Arc::new(crate::rig::ProcessRig::new(
            "mk", prog, args,
        )));
        reg.set_default(Some("mk".to_string()));
        reg
    }

    #[test]
    fn artifact_scan_records_created_file_empty_mode() {
        let (s, _tmp) = store_ws();
        let id = ready_brief(&s, "make a file", "agt_a");
        let report = run_brief_now(
            &s,
            &file_creating_rig("note.txt", "hello"),
            None,
            300,
            &id,
            None,
            "x".into(),
        )
        .unwrap();
        let run_id = report.run_id.unwrap();
        let arts = s.list_run_artifacts(&run_id).unwrap();
        // note.txt is recorded as created; BRIEF.md (unchanged) is NOT.
        assert!(
            arts.iter()
                .any(|a| a.rel_path == "note.txt" && a.kind == "created")
        );
        assert!(!arts.iter().any(|a| a.rel_path == "BRIEF.md"));
        // The transcript announces the scan + counts.
        let kinds: Vec<String> = s
            .list_run_events(&run_id, 200)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&"artifacts.scan_started".to_string()));
        assert!(kinds.contains(&"artifacts.detected".to_string()));
    }

    #[test]
    fn scan_manifest_excludes_dangerous_dirs_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        std::fs::write(p.join("keep.txt"), "x").unwrap();
        std::fs::create_dir_all(p.join("sub")).unwrap();
        std::fs::write(p.join("sub").join("nested.rs"), "y").unwrap();
        // Excluded dirs + secret files an agent might leave behind:
        std::fs::create_dir_all(p.join(".git")).unwrap();
        std::fs::write(p.join(".git").join("HEAD"), "ref").unwrap();
        std::fs::create_dir_all(p.join("target")).unwrap();
        std::fs::write(p.join("target").join("out"), "bin").unwrap();
        std::fs::create_dir_all(p.join("node_modules")).unwrap();
        std::fs::write(p.join("node_modules").join("d.js"), "z").unwrap();
        std::fs::create_dir_all(p.join("dev-data")).unwrap();
        std::fs::write(p.join("dev-data").join("tasks.db"), "db").unwrap();
        std::fs::write(p.join(".env"), "SECRET=1").unwrap();
        std::fs::write(p.join("api.key"), "k").unwrap();

        let m = scan_workspace_manifest(p);
        let paths: Vec<&String> = m.files.keys().collect();
        assert!(m.files.contains_key("keep.txt"));
        assert!(m.files.contains_key("sub/nested.rs"));
        for bad in [
            ".git/HEAD",
            "target/out",
            "node_modules/d.js",
            "dev-data/tasks.db",
            ".env",
            "api.key",
        ] {
            assert!(
                !m.files.contains_key(bad),
                "{bad} should be excluded; got {paths:?}"
            );
        }
    }

    #[test]
    fn diff_manifests_detects_created_modified_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        std::fs::write(p.join("a.txt"), "a").unwrap();
        std::fs::write(p.join("b.txt"), "b").unwrap();
        let before = scan_workspace_manifest(p);
        // mutate: modify a, delete b, create c.
        std::fs::write(p.join("a.txt"), "a-changed").unwrap();
        std::fs::remove_file(p.join("b.txt")).unwrap();
        std::fs::write(p.join("c.txt"), "c").unwrap();
        let after = scan_workspace_manifest(p);
        let changes = diff_manifests(&before, &after);
        let by: std::collections::HashMap<_, _> = changes
            .iter()
            .map(|c| (c.rel_path.as_str(), c.kind))
            .collect();
        assert_eq!(by.get("a.txt"), Some(&"modified"));
        assert_eq!(by.get("b.txt"), Some(&"deleted"));
        assert_eq!(by.get("c.txt"), Some(&"created"));
    }

    #[test]
    fn artifact_preview_refuses_binary_truncates_and_redacts() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_string_lossy().into_owned();
        // text + secret → redacted
        let secret = "sk-".to_string() + &"a".repeat(40);
        std::fs::write(tmp.path().join("t.txt"), format!("key {secret} end")).unwrap();
        match read_artifact_preview(&ws, "t.txt", true, 1024) {
            PreviewOutcome::Text { content, truncated } => {
                assert!(!truncated);
                assert!(!content.contains(&secret), "secret must be redacted");
            }
            o => panic!("expected Text, got {o:?}"),
        }
        // binary (NUL byte) → refused
        std::fs::write(tmp.path().join("b.bin"), [0u8, 1, 2, 3]).unwrap();
        assert_eq!(
            read_artifact_preview(&ws, "b.bin", false, 1024),
            PreviewOutcome::Binary
        );
        // a flagged-text file that is actually binary → still refused
        assert_eq!(
            read_artifact_preview(&ws, "b.bin", true, 1024),
            PreviewOutcome::Binary
        );
        // large text → truncated
        std::fs::write(tmp.path().join("big.txt"), "x".repeat(5000)).unwrap();
        match read_artifact_preview(&ws, "big.txt", true, 1000) {
            PreviewOutcome::Text { content, truncated } => {
                assert!(truncated);
                assert!(content.len() <= 1000);
            }
            o => panic!("expected truncated Text, got {o:?}"),
        }
        // path traversal → Missing/Unsafe, never escaping the workspace
        assert!(matches!(
            read_artifact_preview(&ws, "../escape.txt", true, 1024),
            PreviewOutcome::Missing | PreviewOutcome::Unsafe
        ));
    }

    #[test]
    fn read_artifact_diff_created_modified_deleted_and_safe_refusals() {
        let ws = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let wss = ws.path().to_string_lossy().into_owned();
        let max = 64 * 1024;

        // created → diff against an EMPTY baseline (all additions).
        std::fs::write(ws.path().join("new.txt"), "line1\nline2\n").unwrap();
        match read_artifact_diff(&wss, proj.path(), "new.txt", "created", true, None, max) {
            DiffOutcome::Unified {
                diff,
                baseline,
                truncated,
            } => {
                assert_eq!(baseline, "empty");
                assert!(!truncated);
                assert!(diff.contains("+line1"), "diff: {diff}");
            }
            DiffOutcome::Unavailable { reason } => panic!("expected diff, got: {reason}"),
        }

        // modified → diff against the live project file (still == baseline hash).
        std::fs::write(proj.path().join("m.txt"), "old\n").unwrap();
        let bh = hash_file_hex(&proj.path().join("m.txt")).unwrap();
        std::fs::write(ws.path().join("m.txt"), "new\n").unwrap();
        match read_artifact_diff(&wss, proj.path(), "m.txt", "modified", true, Some(&bh), max) {
            DiffOutcome::Unified { diff, baseline, .. } => {
                assert_eq!(baseline, "project_root");
                assert!(diff.contains("-old"), "diff: {diff}");
                assert!(diff.contains("+new"), "diff: {diff}");
            }
            DiffOutcome::Unavailable { reason } => panic!("expected diff, got: {reason}"),
        }

        // deleted → baseline vs empty (all removals).
        std::fs::write(proj.path().join("d.txt"), "gone\n").unwrap();
        let dh = hash_file_hex(&proj.path().join("d.txt")).unwrap();
        match read_artifact_diff(&wss, proj.path(), "d.txt", "deleted", true, Some(&dh), max) {
            DiffOutcome::Unified { diff, baseline, .. } => {
                assert_eq!(baseline, "project_root");
                assert!(diff.contains("-gone"), "diff: {diff}");
            }
            DiffOutcome::Unavailable { reason } => panic!("expected diff, got: {reason}"),
        }

        // moved baseline → honest unavailable, NEVER a misleading diff.
        std::fs::write(proj.path().join("m.txt"), "DIVERGED\n").unwrap();
        match read_artifact_diff(&wss, proj.path(), "m.txt", "modified", true, Some(&bh), max) {
            DiffOutcome::Unavailable { reason } => {
                assert!(
                    reason.contains("changed since this run"),
                    "reason: {reason}"
                )
            }
            DiffOutcome::Unified { .. } => panic!("expected unavailable (baseline moved)"),
        }

        // binary → unavailable (never dumps bytes).
        match read_artifact_diff(&wss, proj.path(), "new.txt", "created", false, None, max) {
            DiffOutcome::Unavailable { reason } => assert!(reason.contains("binary")),
            DiffOutcome::Unified { .. } => panic!("binary must not diff"),
        }

        // path traversal → refused before any read.
        match read_artifact_diff(
            &wss,
            proj.path(),
            "../escape.txt",
            "created",
            true,
            None,
            max,
        ) {
            DiffOutcome::Unavailable { reason } => assert!(reason.contains("path refused")),
            DiffOutcome::Unified { .. } => panic!("traversal must be refused"),
        }

        // large output → truncated + bounded.
        std::fs::write(ws.path().join("big.txt"), "y\n".repeat(50_000)).unwrap();
        match read_artifact_diff(&wss, proj.path(), "big.txt", "created", true, None, 1000) {
            DiffOutcome::Unified {
                diff, truncated, ..
            } => {
                assert!(truncated);
                assert!(diff.len() <= 1000);
            }
            DiffOutcome::Unavailable { reason } => panic!("expected bounded diff, got: {reason}"),
        }
    }

    #[test]
    fn review_only_accepts_done_runs() {
        let (s, _tmp) = store_ws();
        let id = ready_brief(&s, "reviewable", "agt_a");
        let report = run_brief_now(
            &s,
            &echo_registry(),
            None,
            300,
            &id,
            Some("echo"),
            "x".into(),
        )
        .unwrap();
        let run_id = report.run_id.unwrap();
        // A done run opens pending_review.
        assert_eq!(
            s.get_run(&run_id).unwrap().unwrap().review.as_deref(),
            Some("pending_review")
        );
        // accept it.
        assert_eq!(
            s.set_run_review(&run_id, "accepted", "looks good").unwrap(),
            "accepted"
        );
        let rec = s.get_run(&run_id).unwrap().unwrap();
        assert_eq!(rec.review.as_deref(), Some("accepted"));
        assert_eq!(rec.review_note.as_deref(), Some("looks good"));
        assert!(rec.reviewed_at.is_some());
        // invalid decision is rejected.
        assert!(s.set_run_review(&run_id, "maybe", "").is_err());
    }

    #[test]
    fn discard_run_marks_discarded_rejects_review_and_refuses_running() {
        let (s, _tmp) = store_ws();
        let id = ready_brief(&s, "discardable", "agt_a");
        let report = run_brief_now(
            &s,
            &echo_registry(),
            None,
            300,
            &id,
            Some("echo"),
            "x".into(),
        )
        .unwrap();
        let run_id = report.run_id.unwrap();

        // Discard a `done` run → apply_status `discarded` + review `rejected`.
        assert_eq!(s.discard_run(&run_id).unwrap(), "discarded");
        let rec = s.get_run(&run_id).unwrap().unwrap();
        assert_eq!(rec.apply_status.as_deref(), Some("discarded"));
        assert_eq!(rec.review.as_deref(), Some("rejected"));
        // …and it can NEVER be applied.
        assert!(run_apply_eligibility(&rec).is_err());

        // A `discarded` transcript event + a brief.run_discarded Chronicle note.
        let kinds: Vec<String> = s
            .list_run_events(&run_id, 50)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&"discarded".to_string()), "kinds: {kinds:?}");
        let chron = s
            .query_events(
                &id,
                0,
                50,
                Some("brief.run_discarded"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert_eq!(chron.len(), 1);

        // A RUNNING run cannot be discarded (cancel it first).
        s.record_run_start(
            "run_live",
            &id,
            "agt_a",
            "echo",
            "manual",
            &crate::nodes::coordinator::RunWorkspaceInfo::default(),
        )
        .unwrap();
        assert!(s.discard_run("run_live").is_err());
        // Unknown run → error.
        assert!(s.discard_run("nope").is_err());
    }

    #[test]
    fn run_belongs_to_tenant_isolates_guilds() {
        let s = store();
        // A Brief in guild-a + a run on it.
        let id = s
            .create("t", "f", "{}", "subj", RetryPolicy::None, 0, None, None)
            .unwrap();
        s.set_task_tenant(&id, "guild-a").unwrap();
        s.record_run_start(
            "run_xyz",
            &id,
            "agt",
            "echo",
            "manual",
            &crate::nodes::coordinator::RunWorkspaceInfo::default(),
        )
        .unwrap();
        assert!(s.run_belongs_to_tenant("run_xyz", "guild-a").unwrap());
        assert!(
            !s.run_belongs_to_tenant("run_xyz", "guild-b").unwrap(),
            "no cross-tenant access"
        );
        assert!(
            !s.run_belongs_to_tenant("run_missing", "guild-a").unwrap(),
            "missing run = false"
        );
    }

    // ── Safe-apply tests ───────────────────────────────────────────────

    /// Content hash mirroring [`file_sig`] so a test can pin the exact
    /// baseline/source hash an artifact would carry.
    fn content_hash(bytes: &[u8]) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        format!("{:016x}", h.finish())
    }

    fn make_symlink(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link)
        }
    }

    /// A `done` + `accepted` run with a scoped workspace at `ws`. Returns
    /// `(run_id, brief_id)`.
    fn accepted_run(s: &TaskStore, ws: &std::path::Path) -> (String, String) {
        let brief = s
            .create(
                "apply brief",
                "f",
                "{}",
                "subj",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        let run_id = "run_apply".to_string();
        let wss = ws.to_string_lossy().into_owned();
        let info = crate::nodes::coordinator::RunWorkspaceInfo {
            path: Some(&wss),
            context: Some("copy_repo"),
            files: None,
            bytes: None,
        };
        s.record_run_start(&run_id, &brief, "agt", "echo", "manual", &info)
            .unwrap();
        s.record_run_finish(&run_id, "done", "ok").unwrap();
        s.set_run_review(&run_id, "accepted", "").unwrap();
        (run_id, brief)
    }

    /// Record one artifact; writes its source file into the workspace when
    /// `content` is `Some` (created/modified), leaving baseline as given.
    #[allow(clippy::too_many_arguments)]
    fn put_artifact(
        s: &TaskStore,
        run_id: &str,
        brief: &str,
        ws: &std::path::Path,
        rel: &str,
        kind: &str,
        content: Option<&[u8]>,
        baseline: Option<&str>,
    ) {
        if let Some(c) = content {
            let p = ws.join(rel);
            if let Some(par) = p.parent() {
                std::fs::create_dir_all(par).unwrap();
            }
            std::fs::write(&p, c).unwrap();
        }
        let hash = content.map(content_hash);
        let size = content.map(|c| c.len() as i64).unwrap_or(0);
        let wss = ws.to_string_lossy().into_owned();
        s.record_run_artifact(
            run_id,
            brief,
            &wss,
            rel,
            kind,
            size,
            hash.as_deref(),
            baseline,
            true,
        )
        .unwrap();
    }

    #[test]
    fn apply_eligibility_gates_non_applicable_runs() {
        let s = store();
        let ws = tempfile::tempdir().unwrap();
        let brief = s
            .create("b", "f", "{}", "subj", RetryPolicy::None, 0, None, None)
            .unwrap();
        let wss = ws.path().to_string_lossy().into_owned();
        let info = crate::nodes::coordinator::RunWorkspaceInfo {
            path: Some(&wss),
            context: Some("copy_repo"),
            files: None,
            bytes: None,
        };
        s.record_run_start("r1", &brief, "a", "echo", "manual", &info)
            .unwrap();
        // running → ineligible
        assert!(run_apply_eligibility(&s.get_run("r1").unwrap().unwrap()).is_err());
        // done but pending_review → ineligible
        s.record_run_finish("r1", "done", "ok").unwrap();
        assert!(run_apply_eligibility(&s.get_run("r1").unwrap().unwrap()).is_err());
        // accepted → eligible
        s.set_run_review("r1", "accepted", "").unwrap();
        assert!(run_apply_eligibility(&s.get_run("r1").unwrap().unwrap()).is_ok());
        // rejected → ineligible
        s.set_run_review("r1", "rejected", "nah").unwrap();
        assert!(run_apply_eligibility(&s.get_run("r1").unwrap().unwrap()).is_err());
        // inherit-mode (no scoped workspace) done+accepted → ineligible
        s.record_run_start(
            "r2",
            &brief,
            "a",
            "echo",
            "manual",
            &crate::nodes::coordinator::RunWorkspaceInfo::default(),
        )
        .unwrap();
        s.record_run_finish("r2", "done", "ok").unwrap();
        s.set_run_review("r2", "accepted", "").unwrap();
        assert!(
            run_apply_eligibility(&s.get_run("r2").unwrap().unwrap()).is_err(),
            "inherit-mode run has no workspace to apply"
        );
    }

    #[test]
    fn empty_artifacts_plan_is_a_clear_noop() {
        let proj = tempfile::tempdir().unwrap();
        let plan = build_apply_plan(proj.path(), &[]).unwrap();
        assert!(plan.applicable);
        assert_eq!(plan.changes, 0);
        assert!(plan.note.contains("nothing to apply"));
    }

    #[test]
    fn apply_creates_new_file_and_is_idempotent() {
        let s = store();
        let ws = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let (run, brief) = accepted_run(&s, ws.path());
        put_artifact(
            &s,
            &run,
            &brief,
            ws.path(),
            "out/new.txt",
            "created",
            Some(b"hello"),
            None,
        );
        let arts = s.list_run_artifacts(&run).unwrap();

        let plan = build_apply_plan(proj.path(), &arts).unwrap();
        assert!(plan.applicable);
        assert_eq!(plan.changes, 1);

        let out = apply_run(proj.path(), &arts).unwrap();
        assert_eq!(out.status, "applied");
        assert_eq!(out.applied_files, 1);
        assert_eq!(
            std::fs::read_to_string(proj.path().join("out/new.txt")).unwrap(),
            "hello"
        );

        // Re-apply: target now identical → all noop, nothing rewritten.
        let out2 = apply_run(proj.path(), &arts).unwrap();
        assert_eq!(out2.status, "applied");
        assert_eq!(out2.applied_files, 0);
        assert_eq!(
            std::fs::read_to_string(proj.path().join("out/new.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn apply_created_refuses_when_target_differs() {
        let s = store();
        let ws = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let (run, brief) = accepted_run(&s, ws.path());
        put_artifact(
            &s,
            &run,
            &brief,
            ws.path(),
            "f.txt",
            "created",
            Some(b"new"),
            None,
        );
        std::fs::write(proj.path().join("f.txt"), "OLD different").unwrap();
        let arts = s.list_run_artifacts(&run).unwrap();

        let plan = build_apply_plan(proj.path(), &arts).unwrap();
        assert!(!plan.applicable);
        assert_eq!(plan.conflicts, 1);

        // The whole apply is refused; the existing target is untouched.
        let out = apply_run(proj.path(), &arts).unwrap();
        assert_eq!(out.status, "conflicted");
        assert_eq!(out.applied_files, 0);
        assert_eq!(
            std::fs::read_to_string(proj.path().join("f.txt")).unwrap(),
            "OLD different"
        );
    }

    #[test]
    fn apply_modified_refuses_when_baseline_unverifiable() {
        let s = store();
        let ws = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let (run, brief) = accepted_run(&s, ws.path());
        std::fs::write(proj.path().join("m.txt"), "old").unwrap();
        // No baseline hash recorded → cannot prove the target is unchanged.
        put_artifact(
            &s,
            &run,
            &brief,
            ws.path(),
            "m.txt",
            "modified",
            Some(b"new"),
            None,
        );
        let arts = s.list_run_artifacts(&run).unwrap();

        let plan = build_apply_plan(proj.path(), &arts).unwrap();
        assert!(!plan.applicable);
        assert_eq!(plan.conflicts, 1);
    }

    #[test]
    fn apply_modified_overwrites_when_baseline_matches() {
        let s = store();
        let ws = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let (run, brief) = accepted_run(&s, ws.path());
        std::fs::write(proj.path().join("m.txt"), "old").unwrap();
        let base = content_hash(b"old");
        put_artifact(
            &s,
            &run,
            &brief,
            ws.path(),
            "m.txt",
            "modified",
            Some(b"new"),
            Some(&base),
        );
        let arts = s.list_run_artifacts(&run).unwrap();

        let plan = build_apply_plan(proj.path(), &arts).unwrap();
        assert!(plan.applicable);
        assert_eq!(plan.changes, 1);

        let out = apply_run(proj.path(), &arts).unwrap();
        assert_eq!(out.status, "applied");
        assert_eq!(
            std::fs::read_to_string(proj.path().join("m.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn apply_deleted_requires_baseline_match() {
        let s = store();
        let ws = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let (run, brief) = accepted_run(&s, ws.path());
        // Target diverged from the run's baseline → refuse to delete.
        std::fs::write(proj.path().join("d.txt"), "current").unwrap();
        let base = content_hash(b"original");
        put_artifact(
            &s,
            &run,
            &brief,
            ws.path(),
            "d.txt",
            "deleted",
            None,
            Some(&base),
        );
        let arts = s.list_run_artifacts(&run).unwrap();

        let plan = build_apply_plan(proj.path(), &arts).unwrap();
        assert!(!plan.applicable);
        assert_eq!(plan.conflicts, 1);
        let out = apply_run(proj.path(), &arts).unwrap();
        assert_eq!(out.status, "conflicted");
        assert!(
            proj.path().join("d.txt").exists(),
            "target must survive a refused delete"
        );
    }

    #[test]
    fn apply_deleted_removes_when_baseline_matches_and_is_idempotent() {
        let s = store();
        let ws = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let (run, brief) = accepted_run(&s, ws.path());
        std::fs::write(proj.path().join("d.txt"), "original").unwrap();
        let base = content_hash(b"original");
        put_artifact(
            &s,
            &run,
            &brief,
            ws.path(),
            "d.txt",
            "deleted",
            None,
            Some(&base),
        );
        let arts = s.list_run_artifacts(&run).unwrap();

        let out = apply_run(proj.path(), &arts).unwrap();
        assert_eq!(out.status, "applied");
        assert_eq!(out.applied_files, 1);
        assert!(!proj.path().join("d.txt").exists());

        // Re-apply: already absent → noop, no error.
        let out2 = apply_run(proj.path(), &arts).unwrap();
        assert_eq!(out2.status, "applied");
        assert_eq!(out2.applied_files, 0);
    }

    #[test]
    fn apply_refuses_unsafe_and_excluded_paths() {
        let s = store();
        let ws = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let (run, brief) = accepted_run(&s, ws.path());
        let wss = ws.path().to_string_lossy().into_owned();
        for rel in [
            "../escape.txt",
            "C:/windows/x",
            ".git/config",
            "node_modules/x.js",
            ".env",
        ] {
            s.record_run_artifact(
                &run,
                &brief,
                &wss,
                rel,
                "created",
                1,
                Some("dead"),
                None,
                true,
            )
            .unwrap();
        }
        let arts = s.list_run_artifacts(&run).unwrap();
        let plan = build_apply_plan(proj.path(), &arts).unwrap();
        assert!(!plan.applicable);
        assert!(
            plan.items.iter().all(|i| !i.can_apply),
            "every unsafe/excluded path is refused"
        );
        assert_eq!(plan.changes, 0);
    }

    #[test]
    fn apply_refuses_symlinked_target() {
        let s = store();
        let ws = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let (run, brief) = accepted_run(&s, ws.path());
        let outside = ws.path().join("real.txt");
        std::fs::write(&outside, "secret").unwrap();
        let link = proj.path().join("link.txt");
        if make_symlink(&outside, &link).is_err() {
            return; // platform can't create symlinks here — skip.
        }
        put_artifact(
            &s,
            &run,
            &brief,
            ws.path(),
            "link.txt",
            "created",
            Some(b"new"),
            None,
        );
        let arts = s.list_run_artifacts(&run).unwrap();
        let plan = build_apply_plan(proj.path(), &arts).unwrap();
        assert!(!plan.applicable, "must refuse writing through a symlink");
        // The symlink's real target content is untouched.
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "secret");
    }

    #[test]
    fn apply_status_persists_on_the_run() {
        let s = store();
        let ws = tempfile::tempdir().unwrap();
        let (run, _) = accepted_run(&s, ws.path());
        s.set_run_apply_status(&run, "applied", "2 applied, 0 failed", 2, 0)
            .unwrap();
        let r = s.get_run(&run).unwrap().unwrap();
        assert_eq!(r.apply_status.as_deref(), Some("applied"));
        assert_eq!(r.applied_files, Some(2));
        assert_eq!(r.failed_files, Some(0));
        assert!(r.applied_at.is_some());
        assert!(s.set_run_apply_status("nope", "applied", "", 0, 0).is_err());
    }

    #[test]
    fn apply_end_to_end_from_a_real_run_into_a_disposable_root() {
        // Capstone: a REAL run (real scoped workspace + real adapter process
        // writing a file) → artifacts with baseline → accept → plan → apply
        // into a DISPOSABLE temp project root (never the repo) → re-apply
        // proves idempotency. The deterministic equivalent of the live smoke.
        let (s, _tmp) = store_ws();
        let id = ready_brief(&s, "write a note", "agt_a");
        let report = run_brief_now(
            &s,
            &file_creating_rig("applied_note.txt", "hello"),
            None,
            300,
            &id,
            None,
            "x".into(),
        )
        .unwrap();
        let run_id = report.run_id.unwrap();
        // The run produced a real created artifact in its scoped workspace.
        let arts = s.list_run_artifacts(&run_id).unwrap();
        assert!(
            arts.iter()
                .any(|a| a.rel_path == "applied_note.txt" && a.kind == "created")
        );

        // Accept it, then apply into a disposable project root.
        s.set_run_review(&run_id, "accepted", "ok").unwrap();
        assert!(run_apply_eligibility(&s.get_run(&run_id).unwrap().unwrap()).is_ok());
        let proj = tempfile::tempdir().unwrap();

        let plan = build_apply_plan(proj.path(), &arts).unwrap();
        assert!(
            plan.applicable,
            "a clean created file must be applicable: {plan:?}"
        );
        assert!(plan.changes >= 1);

        let out = apply_run(proj.path(), &arts).unwrap();
        assert_eq!(out.status, "applied");
        assert!(out.applied_files >= 1);
        let landed = proj.path().join("applied_note.txt");
        assert!(
            landed.exists(),
            "the file must land in the disposable project root"
        );
        let body = std::fs::read(&landed).unwrap();
        assert!(!body.is_empty());

        // Re-apply: target now identical → all noop, 0 writes (idempotent).
        let out2 = apply_run(proj.path(), &arts).unwrap();
        assert_eq!(out2.status, "applied");
        assert_eq!(out2.applied_files, 0);
        assert_eq!(
            std::fs::read(&landed).unwrap(),
            body,
            "idempotent re-apply must not corrupt"
        );
    }

    #[test]
    fn apply_plan_is_tenant_scoped() {
        let s = store();
        let brief = s
            .create("t", "f", "{}", "subj", RetryPolicy::None, 0, None, None)
            .unwrap();
        s.set_task_tenant(&brief, "guild-a").unwrap();
        s.record_run_start(
            "rA",
            &brief,
            "a",
            "echo",
            "manual",
            &crate::nodes::coordinator::RunWorkspaceInfo::default(),
        )
        .unwrap();
        // The diff/apply capabilities gate on this exact check.
        assert!(s.run_belongs_to_tenant("rA", "guild-a").unwrap());
        assert!(
            !s.run_belongs_to_tenant("rA", "guild-b").unwrap(),
            "guild-b cannot diff/apply guild-a's run"
        );
    }

    // ── STAGE-2 guarded operator retry (execution-and-issue §3.3b) ──

    /// Record a terminal source Shift with an explicit recovery diagnosis so a
    /// retry pre-check has something honest to read. `retryable` chooses a
    /// transient (retryable) vs permanent (non-retryable) `failed` run.
    fn failed_source(s: &TaskStore, brief: &str, agent: &str, run_id: &str, retryable: bool) {
        use crate::nodes::coordinator::{RunDiagnosis, RunWorkspaceInfo};
        s.record_run_start(
            run_id,
            brief,
            agent,
            "echo",
            "manual",
            &RunWorkspaceInfo::default(),
        )
        .unwrap();
        s.record_run_finish(run_id, "failed", "boom").unwrap();
        // `record_run_finish` stamps a conservative non-retryable diagnosis; for
        // the retryable case re-stamp it as a transient (retry-may-help) failure.
        if retryable {
            s.set_run_diagnosis(
                run_id,
                &RunDiagnosis::for_terminal("failed", Some(true), run_id),
            )
            .unwrap();
        }
    }

    fn count_children(s: &TaskStore, source: &str) -> i64 {
        let conn = s.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM brief_runs WHERE retried_from_run_id = ?1",
            rusqlite::params![source],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn retry_opens_exactly_one_child_and_links_lineage() {
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let reg = RigRegistry::with_builtins();
        let brief = ready_brief(&s, "ship it", "agt_a");
        let src = "run_src1";
        failed_source(&s, &brief, "agt_a", src, true);

        let (child, attempt) = match open_retry_child(
            &s,
            &reg,
            None,
            300,
            src,
            "default",
            "retry prompt".into(),
            Some("echo"),
            RunModelPrefs::default(),
        )
        .unwrap()
        {
            RetryOpen::Ready {
                child_run_id,
                attempt,
                ready,
                source_run_id,
            } => {
                assert_eq!(source_run_id, src);
                // Finish the child so the Claim is released (echo → done).
                let _ = execute_ready(&s, None, *ready);
                (child_run_id, attempt)
            }
            _ => panic!("a retryable failed Shift must open a retry child"),
        };

        assert_ne!(child, src, "the child is a distinct run");
        assert_eq!(attempt, 1, "first retry is attempt 1");
        let cr = s.get_run(&child).unwrap().unwrap();
        assert_eq!(
            cr.retried_from_run_id.as_deref(),
            Some(src),
            "lineage linked"
        );
        assert_eq!(cr.retry_attempt, Some(1));
        assert_eq!(
            s.existing_retry_child(src).unwrap().as_deref(),
            Some(child.as_str())
        );
        assert_eq!(count_children(&s, src), 1, "exactly one child opened");
        // The retry was chronicled on the Brief.
        let evs = s
            .query_events(
                &brief,
                0,
                50,
                Some("brief.retry_requested"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert_eq!(evs.len(), 1, "the retry is chronicled on the Brief");
    }

    #[test]
    fn duplicate_retry_returns_existing_child_without_spawning_another() {
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let reg = RigRegistry::with_builtins();
        let brief = ready_brief(&s, "x", "agt_a");
        let src = "run_src2";
        failed_source(&s, &brief, "agt_a", src, true);

        let child1 = match open_retry_child(
            &s,
            &reg,
            None,
            300,
            src,
            "default",
            "p".into(),
            Some("echo"),
            RunModelPrefs::default(),
        )
        .unwrap()
        {
            RetryOpen::Ready {
                child_run_id,
                ready,
                ..
            } => {
                let _ = execute_ready(&s, None, *ready);
                child_run_id
            }
            _ => panic!("first retry must open a child"),
        };

        // Second retry of the SAME source: the duplicate guard returns the
        // existing child id and opens NO new run.
        match open_retry_child(
            &s,
            &reg,
            None,
            300,
            src,
            "default",
            "p".into(),
            Some("echo"),
            RunModelPrefs::default(),
        )
        .unwrap()
        {
            RetryOpen::AlreadyRetried { child_run_id } => assert_eq!(child_run_id, child1),
            _ => panic!("a duplicate retry must return the existing child"),
        }
        assert_eq!(count_children(&s, src), 1, "no second child was spawned");
    }

    #[test]
    fn non_retryable_failed_run_refuses_retry() {
        use crate::nodes::coordinator::RetryPrecheck;
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let reg = RigRegistry::with_builtins();
        let brief = ready_brief(&s, "x", "agt_a");
        let src = "run_perm";
        failed_source(&s, &brief, "agt_a", src, false); // permanent → not retryable

        match s.retry_precheck(src, "default").unwrap() {
            RetryPrecheck::Refused { status, .. } => assert_eq!(status, "not_retryable"),
            _ => panic!("a permanent failure must refuse retry"),
        }
        match open_retry_child(
            &s,
            &reg,
            None,
            300,
            src,
            "default",
            "p".into(),
            Some("echo"),
            RunModelPrefs::default(),
        )
        .unwrap()
        {
            RetryOpen::Refused(report) => assert_eq!(report.status, "not_retryable"),
            _ => panic!("open_retry_child must surface the refusal"),
        }
        assert!(
            s.existing_retry_child(src).unwrap().is_none(),
            "a refused retry opens no child"
        );
        assert_eq!(count_children(&s, src), 0);
    }

    #[test]
    fn cross_tenant_retry_denied_as_not_found() {
        use crate::nodes::coordinator::{RetryPrecheck, RunDiagnosis, RunWorkspaceInfo};
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let reg = RigRegistry::with_builtins();
        let brief = s
            .create("t", "f", "{}", "subj", RetryPolicy::None, 0, None, None)
            .unwrap();
        s.set_task_tenant(&brief, "guild-a").unwrap();
        s.set_brief_field(&brief, "assignee", "agt_a").unwrap();
        let src = "run_iso";
        s.record_run_start(
            src,
            &brief,
            "agt_a",
            "echo",
            "manual",
            &RunWorkspaceInfo::default(),
        )
        .unwrap();
        s.record_run_finish(src, "failed", "boom").unwrap();
        s.set_run_diagnosis(src, &RunDiagnosis::for_terminal("failed", Some(true), src))
            .unwrap();

        // Same Guild: eligible.
        assert!(matches!(
            s.retry_precheck(src, "guild-a").unwrap(),
            RetryPrecheck::Eligible { .. }
        ));
        // Cross Guild: reads as not-found — no existence leak, no dispatch.
        assert_eq!(
            s.retry_precheck(src, "guild-b").unwrap(),
            RetryPrecheck::NotFound
        );
        match open_retry_child(
            &s,
            &reg,
            None,
            300,
            src,
            "guild-b",
            "p".into(),
            Some("echo"),
            RunModelPrefs::default(),
        )
        .unwrap()
        {
            RetryOpen::NotFound => {}
            _ => panic!("a cross-tenant retry must read as not-found"),
        }
        assert!(s.existing_retry_child(src).unwrap().is_none());
    }

    #[test]
    fn retry_child_inherits_operative_model_prefs() {
        // A guarded operator retry must run the child Shift on the SAME model
        // the assigned Operative is configured for — not a silent downgrade to
        // the adapter default (execution-and-issue §3.3b / adapters §3.2/§3.3).
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mut reg = RigRegistry::with_builtins();
        reg.register(Arc::new(CaptureRig { seen: seen.clone() }));
        let brief = ready_brief(&s, "ship it", "agt_a");
        let src = "run_pref_src";
        failed_source(&s, &brief, "agt_a", src, true);

        match open_retry_child(
            &s,
            &reg,
            None,
            300,
            src,
            "default",
            "retry prompt".into(),
            Some("capture"),
            RunModelPrefs::new(
                Some("claude-sonnet-4".to_string()),
                Some("high".to_string()),
            ),
        )
        .unwrap()
        {
            RetryOpen::Ready { ready, .. } => {
                let _ = execute_ready(&s, None, *ready);
            }
            _ => panic!("a retryable failed Shift must open a retry child"),
        }
        let got = seen
            .lock()
            .unwrap()
            .clone()
            .expect("the retry child ran the Rig");
        assert_eq!(
            got.0.as_deref(),
            Some("claude-sonnet-4"),
            "retry child inherits the model pref"
        );
        assert_eq!(
            got.1.as_deref(),
            Some("high"),
            "retry child inherits the reasoning effort"
        );
    }

    #[test]
    fn retry_child_stays_clean_when_operative_has_no_prefs() {
        // No stored preference → the retry child's request carries neither hint
        // (the Rig runs on its own default). Proves the absent path stays clean.
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let mut reg = RigRegistry::with_builtins();
        reg.register(Arc::new(CaptureRig { seen: seen.clone() }));
        let brief = ready_brief(&s, "ship it", "agt_a");
        let src = "run_pref_src2";
        failed_source(&s, &brief, "agt_a", src, true);

        match open_retry_child(
            &s,
            &reg,
            None,
            300,
            src,
            "default",
            "retry prompt".into(),
            Some("capture"),
            RunModelPrefs::default(),
        )
        .unwrap()
        {
            RetryOpen::Ready { ready, .. } => {
                let _ = execute_ready(&s, None, *ready);
            }
            _ => panic!("a retryable failed Shift must open a retry child"),
        }
        let got = seen
            .lock()
            .unwrap()
            .clone()
            .expect("the retry child ran the Rig");
        assert_eq!(got.0, None, "no model pref when the Operative has none");
        assert_eq!(got.1, None, "no effort when the Operative has none");
    }

    // ── Stage-2 OPT-IN autonomous retry lane (execution-and-issue §3.3 / §3.3b) ──

    /// A `decide` closure that always retries a candidate on the built-in echo
    /// Rig with a fixed prompt + no model prefs — the test stand-in for the
    /// controller's agent-store/budget-aware policy.
    fn proceed_on_echo() -> impl Fn(&crate::nodes::coordinator::RetryCandidate) -> RetryDecision {
        |_cand| {
            RetryDecision::Proceed(RetryInputs {
                preferred_rig: Some("echo".to_string()),
                prompt: "autonomous retry".to_string(),
                prefs: RunModelPrefs::default(),
            })
        }
    }

    #[test]
    fn autonomous_recovery_switch_is_off_and_bounded_by_default() {
        // Default OFF — the lane never runs unless explicitly enabled.
        assert!(!parse_autonomous_recovery_enabled(None));
        assert!(!parse_autonomous_recovery_enabled(Some("")));
        assert!(!parse_autonomous_recovery_enabled(Some("0")));
        assert!(!parse_autonomous_recovery_enabled(Some("off")));
        assert!(parse_autonomous_recovery_enabled(Some("1")));
        assert!(parse_autonomous_recovery_enabled(Some("true")));
        assert!(parse_autonomous_recovery_enabled(Some(" On ")));
        // Bounded — default 1, clamped to 1..=10 (never 0, never unbounded).
        assert_eq!(parse_autonomous_recovery_max(None), 1);
        assert_eq!(parse_autonomous_recovery_max(Some("3")), 3);
        assert_eq!(parse_autonomous_recovery_max(Some("0")), 1);
        assert_eq!(parse_autonomous_recovery_max(Some("999")), 10);
        assert_eq!(parse_autonomous_recovery_max(Some("nope")), 1);
    }

    #[test]
    fn autonomous_prime_switch_is_off_and_bounded_by_default() {
        // Default OFF — the autonomous Prime driver never runs unless enabled.
        assert!(!parse_autonomous_prime_enabled(None));
        assert!(!parse_autonomous_prime_enabled(Some("")));
        assert!(!parse_autonomous_prime_enabled(Some("0")));
        assert!(!parse_autonomous_prime_enabled(Some("off")));
        assert!(parse_autonomous_prime_enabled(Some("1")));
        assert!(parse_autonomous_prime_enabled(Some("true")));
        assert!(parse_autonomous_prime_enabled(Some("yes")));
        assert!(parse_autonomous_prime_enabled(Some(" On ")));
        // Bounded — default 1, clamped to 1..=10 (never 0, never unbounded).
        assert_eq!(parse_autonomous_prime_max(None), 1);
        assert_eq!(parse_autonomous_prime_max(Some("4")), 4);
        assert_eq!(parse_autonomous_prime_max(Some("0")), 1);
        assert_eq!(parse_autonomous_prime_max(Some("999")), 10);
        assert_eq!(parse_autonomous_prime_max(Some("nope")), 1);
    }

    #[test]
    fn autonomous_recovery_selection_only_includes_eligible_runs() {
        use crate::nodes::coordinator::{RunDiagnosis, RunWorkspaceInfo};
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let reg = RigRegistry::with_builtins();

        // (1) A retryable failed Shift → INCLUDED.
        let b_ok = ready_brief(&s, "ok", "agt_a");
        failed_source(&s, &b_ok, "agt_a", "run_ok", true);

        // (2) A retryable INTERRUPTED Shift → INCLUDED.
        let b_int = ready_brief(&s, "interrupted", "agt_a");
        s.record_run_start(
            "run_int",
            &b_int,
            "agt_a",
            "echo",
            "manual",
            &RunWorkspaceInfo::default(),
        )
        .unwrap();
        s.record_run_finish("run_int", "interrupted", "stalled")
            .unwrap();
        s.set_run_diagnosis(
            "run_int",
            &RunDiagnosis {
                failure_class: Some("interrupted".into()),
                retryable: Some(true),
                retry_budget_remaining: Some(1),
                recovery_action: None,
                recovery_route: None,
            },
        )
        .unwrap();

        // (3) A non-retryable (permanent) failure → EXCLUDED.
        let b_perm = ready_brief(&s, "perm", "agt_a");
        failed_source(&s, &b_perm, "agt_a", "run_perm", false);

        // (4) Retryable but EXHAUSTED budget (0) → EXCLUDED.
        let b_exh = ready_brief(&s, "exhausted", "agt_a");
        failed_source(&s, &b_exh, "agt_a", "run_exh", true);
        s.set_run_diagnosis(
            "run_exh",
            &RunDiagnosis {
                failure_class: Some("transient".into()),
                retryable: Some(true),
                retry_budget_remaining: Some(0),
                recovery_action: None,
                recovery_route: None,
            },
        )
        .unwrap();

        // (5) A refusal (status `refused`) → EXCLUDED (refusals are never retryable).
        let b_ref = ready_brief(&s, "refused", "agt_a");
        s.record_refused_run(&b_ref, "agt_a", "", "no_adapter", "no Rig", "heartbeat")
            .unwrap();

        // (6) A retryable failed Shift that was operator-DISCARDED → EXCLUDED.
        let b_disc = ready_brief(&s, "discarded", "agt_a");
        failed_source(&s, &b_disc, "agt_a", "run_disc", true);
        s.discard_run("run_disc").unwrap();

        // (7) A clean DONE Shift → EXCLUDED.
        let b_done = ready_brief(&s, "done", "agt_a");
        s.record_run_start(
            "run_done",
            &b_done,
            "agt_a",
            "echo",
            "manual",
            &RunWorkspaceInfo::default(),
        )
        .unwrap();
        s.record_run_finish("run_done", "done", "shipped").unwrap();

        // (8) A retryable failed Shift that ALREADY has a retry child → EXCLUDED.
        let b_dup = ready_brief(&s, "already", "agt_a");
        failed_source(&s, &b_dup, "agt_a", "run_dup", true);
        match open_retry_child(
            &s,
            &reg,
            None,
            300,
            "run_dup",
            "default",
            "p".into(),
            Some("echo"),
            RunModelPrefs::default(),
        )
        .unwrap()
        {
            RetryOpen::Ready { ready, .. } => {
                let _ = execute_ready(&s, None, *ready);
            }
            _ => panic!("setup: run_dup must open a child"),
        }

        let picked: std::collections::BTreeSet<String> = s
            .list_autonomous_retry_candidates(None, 100)
            .unwrap()
            .into_iter()
            .map(|c| c.run_id)
            .collect();
        let expected: std::collections::BTreeSet<String> = ["run_ok", "run_int"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            picked, expected,
            "only the retryable, in-budget, un-retried, non-discarded failed/interrupted runs are candidates"
        );
    }

    #[test]
    fn autonomous_recovery_tick_opens_one_child_and_is_idempotent() {
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let reg = RigRegistry::with_builtins();
        let brief = ready_brief(&s, "ship it", "agt_a");
        let src = "run_auto1";
        failed_source(&s, &brief, "agt_a", src, true);

        // First tick: opens + executes EXACTLY one child, chronicled as autonomous.
        let recs =
            autonomous_recovery_tick(&s, &reg, None, 300, 3, None, proceed_on_echo()).unwrap();
        assert_eq!(recs.len(), 1, "one candidate retried");
        assert_eq!(recs[0].outcome, "opened");
        assert_eq!(recs[0].source_run_id, src);
        let child = recs[0].child_run_id.clone().expect("a child was opened");
        assert_eq!(count_children(&s, src), 1, "exactly one child");
        let cr = s.get_run(&child).unwrap().unwrap();
        assert_eq!(
            cr.retried_from_run_id.as_deref(),
            Some(src),
            "lineage linked like operator retry"
        );
        // The autonomous lane chronicles a DISTINCT event kind (not operator).
        let auto = s
            .query_events(
                &brief,
                0,
                50,
                Some("brief.autonomous_retry"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert_eq!(auto.len(), 1, "autonomous retry is chronicled distinctly");
        let op = s
            .query_events(
                &brief,
                0,
                50,
                Some("brief.retry_requested"),
                crate::nodes::coordinator::EventOrder::Desc,
            )
            .unwrap();
        assert!(
            op.is_empty(),
            "the autonomous lane is NOT chronicled as an operator retry"
        );

        // Second tick: the source now has a child, so it is no longer a
        // candidate — no second child, idempotent.
        let recs2 =
            autonomous_recovery_tick(&s, &reg, None, 300, 3, None, proceed_on_echo()).unwrap();
        assert!(recs2.is_empty(), "a second tick finds no new candidate");
        assert_eq!(
            count_children(&s, src),
            1,
            "still exactly one child after a second tick"
        );
    }

    #[test]
    fn autonomous_recovery_disabled_lane_creates_no_retry() {
        // The production loop only calls the tick when the switch is on; with the
        // switch OFF (default) the tick is never invoked, so an eligible run is
        // left untouched. This proves the default-off contract end-to-end.
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let reg = RigRegistry::with_builtins();
        let brief = ready_brief(&s, "ship it", "agt_a");
        let src = "run_off";
        failed_source(&s, &brief, "agt_a", src, true);

        if parse_autonomous_recovery_enabled(None) {
            let _ = autonomous_recovery_tick(
                &s,
                &reg,
                None,
                300,
                parse_autonomous_recovery_max(None),
                None,
                proceed_on_echo(),
            )
            .unwrap();
        }
        assert_eq!(
            count_children(&s, src),
            0,
            "no retry is created while the lane is disabled"
        );
        // And an eligible candidate DOES exist — proving it was the switch, not
        // a lack of work, that held the retry back.
        assert_eq!(
            s.list_autonomous_retry_candidates(None, 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn autonomous_recovery_is_tenant_isolated() {
        use crate::rig::RigRegistry;
        let (s, _tmp) = store_ws();
        let reg = RigRegistry::with_builtins();
        let a = ready_brief_in_tenant(&s, "guild-a work", "agt_a", "guild-a");
        let b = ready_brief_in_tenant(&s, "guild-b work", "agt_b", "guild-b");
        failed_source(&s, &a, "agt_a", "run_ga", true);
        failed_source(&s, &b, "agt_b", "run_gb", true);

        // Scoped selection returns ONLY the named Guild's candidate.
        let only_a: Vec<String> = s
            .list_autonomous_retry_candidates(Some("guild-a"), 50)
            .unwrap()
            .into_iter()
            .map(|c| c.run_id)
            .collect();
        assert_eq!(only_a, vec!["run_ga".to_string()]);

        // A recovery tick scoped to guild-a retries guild-a's run only; guild-b
        // is never touched.
        let recs =
            autonomous_recovery_tick(&s, &reg, None, 300, 5, Some("guild-a"), proceed_on_echo())
                .unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].source_run_id, "run_ga");
        assert_eq!(count_children(&s, "run_ga"), 1, "guild-a's run was retried");
        assert_eq!(
            count_children(&s, "run_gb"),
            0,
            "guild-b's run was NOT retried by guild-a's tick"
        );
    }
}
