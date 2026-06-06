//! **Action Center** — compute the operator's next actions from live state.
//!
//! Implements `docs/relix-company-model.md` §5.4 / §8.2 (the Board's home — a
//! single action center showing only *what needs you*, in priority order,
//! **computed from live state, not a notification table**) and
//! `docs/relix-dashboard-design.md` §5 (approvals · alerts/failures · stranded
//! / blocked work).
//!
//! This module is PURE: it owns the [`ActionItem`] shape and the ordering +
//! dedupe rules so they are unit-tested in isolation. The
//! `handle_company_actions` handler gathers the live signals from the EXISTING
//! stores (pending approvals/Clearances, pending hires, the Brief board, the
//! run ledger, the strategy gate) and feeds them here. There is no I/O and no
//! mutation — the whole surface is read-only by construction.

use serde::{Deserialize, Serialize};

use super::store::{AgentProfile, ApprovalRecord, SPAWN_CLEARANCE_METHOD};
use crate::nodes::coordinator::RunRecord;
use crate::nodes::coordinator::brief::BriefCard;
use crate::nodes::coordinator::spine::store::Mandate;

/// The category of an actionable item. Each maps to a way work is stuck or a
/// gate the operator must clear. Ordering between categories is by [`rank`].
///
/// [`rank`]: ActionCategory::rank
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionCategory {
    /// A pending hire/strategy/spawn Clearance the Board must decide.
    Approval,
    /// A pending Operative awaiting activation that has NO Clearance to decide
    /// (a `route=direct` hire) — it needs an explicit `agent.approve_hire`.
    Hire,
    /// A spend/budget signal the Board must weigh (company-model §5.4 / §8.2):
    /// committed Allowance over (or near) the Guild budget, or an active
    /// Operative hard-stopped by a zero Allowance with work waiting. Computed
    /// from EXISTING allowance state — never a fabricated spend figure.
    Budget,
    /// A Shift that failed / was refused / was interrupted — something broke and
    /// needs the operator to inspect or retry.
    FailedOrRefused,
    /// A completed Shift sitting in `pending_review` — work is done and awaits a
    /// disposition (review → apply).
    NeedsReview,
    /// A Brief that can become a Shift right now (assigned to an active
    /// Operative, unblocked, unclaimed). Surfacing it lets the operator move
    /// work forward immediately.
    ReadyToStart,
    /// A Brief that cannot start — blocked on a dependency or missing an
    /// assignee.
    Blocked,
    /// Work stuck too long in an active column with nobody moving it
    /// (informational — the lowest-priority signal).
    Stale,
}

impl ActionCategory {
    /// Stable wire string (matches the serde `snake_case` rename).
    pub fn as_str(self) -> &'static str {
        match self {
            ActionCategory::Approval => "approval",
            ActionCategory::Hire => "hire",
            ActionCategory::Budget => "budget",
            ActionCategory::FailedOrRefused => "failed_or_refused",
            ActionCategory::NeedsReview => "needs_review",
            ActionCategory::ReadyToStart => "ready_to_start",
            ActionCategory::Blocked => "blocked",
            ActionCategory::Stale => "stale",
        }
    }

    /// Ordering rank — LOWER sorts first. Encodes the company-model priority
    /// (company-model §8.2 + the pack brief):
    /// - approvals / hire blockers near the top (they unblock the whole
    ///   company),
    /// - **budget** governance next — a hard-stop blocks ALL of an Operative's
    ///   work and over-commitment is a Board (§5.4) sovereign-control concern,
    ///   so it sits with the approval/hire blockers, above a single broken Shift,
    /// - failed/refused (recovery) before informational stale items,
    /// - ready_to_start before generic blocked items (it can move work forward).
    pub fn rank(self) -> u8 {
        match self {
            ActionCategory::Approval => 0,
            ActionCategory::Hire => 1,
            ActionCategory::Budget => 2,
            ActionCategory::FailedOrRefused => 3,
            ActionCategory::NeedsReview => 4,
            ActionCategory::ReadyToStart => 5,
            ActionCategory::Blocked => 6,
            ActionCategory::Stale => 7,
        }
    }

    /// The coarse severity badge for the dashboard. NOTE: this is the *category
    /// default*; individual items may carry a per-item [`ActionItem::severity`]
    /// (e.g. a *near*-budget warning is Medium while an over-budget item is High).
    pub fn severity(self) -> ActionSeverity {
        match self {
            ActionCategory::Approval
            | ActionCategory::Hire
            | ActionCategory::Budget
            | ActionCategory::FailedOrRefused => ActionSeverity::High,
            ActionCategory::NeedsReview
            | ActionCategory::ReadyToStart
            | ActionCategory::Blocked => ActionSeverity::Medium,
            ActionCategory::Stale => ActionSeverity::Low,
        }
    }
}

/// Coarse severity for the dashboard badge tone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionSeverity {
    High,
    Medium,
    Low,
}

impl ActionSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            ActionSeverity::High => "high",
            ActionSeverity::Medium => "medium",
            ActionSeverity::Low => "low",
        }
    }
}

/// One actionable item in the operator's feed. Carries the underlying object it
/// points at (so the dashboard can deep-link), the recommended action label, a
/// route hint, and timestamps when known. Serialized directly into the
/// `company.actions` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionItem {
    /// Stable, dedupe-friendly id, e.g. `approval:<id>` / `ready:<brief>`.
    pub id: String,
    pub category: ActionCategory,
    pub severity: ActionSeverity,
    pub title: String,
    /// A short plain-language reason this needs the operator.
    pub reason: String,
    /// The underlying object kind: `agent` / `brief` / `mandate` / `run`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_title: Option<String>,
    /// The recommended next action, e.g. "Approve the hire".
    pub action_label: String,
    /// A dashboard route (or API hint) to act, e.g. `/mandates`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
}

impl ActionItem {
    /// The underlying object this item is about — the dedupe identity. Two items
    /// with the same `(target_type, target_id)` are the SAME thing (e.g. a
    /// pending hire AND its spawn Clearance, or a Brief that is both stale and
    /// blocked), so only the most-urgent survives [`finalize`]. An item with no
    /// target is never deduped.
    fn dedupe_key(&self) -> Option<(&str, &str)> {
        match (self.target_type.as_deref(), self.target_id.as_deref()) {
            (Some(t), Some(id)) if !t.is_empty() && !id.is_empty() => Some((t, id)),
            _ => None,
        }
    }
}

/// Order + dedupe a raw item list into the operator's action feed (Part B).
///
/// STABLE + DETERMINISTIC:
/// 1. sort by `(category.rank(), created_at ascending [oldest first], id)`;
/// 2. dedupe by underlying object, keeping the FIRST occurrence — which, after
///    the rank sort, is the most-urgent item for that object (so a pending hire
///    with a Clearance shows as the `approval`, not also as a `hire`).
///
/// An item with no `(target_type, target_id)` is never deduped (it has no
/// shared identity to collapse onto).
pub fn finalize(mut items: Vec<ActionItem>) -> Vec<ActionItem> {
    // None created_at sorts last within a rank (a known wait surfaces first).
    items.sort_by(|a, b| {
        a.category
            .rank()
            .cmp(&b.category.rank())
            .then(
                a.created_at
                    .unwrap_or(i64::MAX)
                    .cmp(&b.created_at.unwrap_or(i64::MAX)),
            )
            .then(a.id.cmp(&b.id))
    });
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        // A more-urgent item for this object already kept → drop this one.
        if let Some((t, id)) = item.dedupe_key()
            && !seen.insert((t.to_string(), id.to_string()))
        {
            continue;
        }
        out.push(item);
    }
    out
}

// ── Item builders (pure, from the live store rows) ───────────────────────────

/// Truncate a title to a bounded, single-line display snippet.
fn snippet(s: &str) -> String {
    let one_line = s.replace(['\n', '\r'], " ");
    let trimmed = one_line.trim();
    if trimmed.chars().count() <= 120 {
        trimmed.to_string()
    } else {
        let mut t: String = trimmed.chars().take(117).collect();
        t.push('…');
        t
    }
}

/// A pending approval / Clearance (company-model §5.5). A spawn Clearance
/// (`agent.activate_hire`) targets the pending hire it activates; any other
/// pending approval targets its own `agent_id` actor.
pub fn approval_item(a: &ApprovalRecord) -> ActionItem {
    let is_spawn = a.method == SPAWN_CLEARANCE_METHOD;
    let (title, action_label, route) = if is_spawn {
        (
            format!("Approve hire — {}", a.agent_id),
            "Approve the hire Clearance".to_string(),
            Some("/agents".to_string()),
        )
    } else {
        (
            format!("Clearance: {}", a.method),
            "Decide the Clearance".to_string(),
            Some("/mandates".to_string()),
        )
    };
    let reason = if a.reason.trim().is_empty() {
        format!("a pending Clearance for `{}` awaits a decision", a.method)
    } else {
        snippet(&a.reason)
    };
    ActionItem {
        id: format!("approval:{}", a.approval_id),
        category: ActionCategory::Approval,
        severity: ActionCategory::Approval.severity(),
        title,
        reason,
        // A spawn Clearance's underlying object IS the pending hire (agent), so
        // it dedupes against the standalone hire item; other Clearances key on
        // their own actor.
        target_type: Some("agent".to_string()),
        target_id: Some(a.agent_id.clone()),
        target_title: None,
        action_label,
        route,
        created_at: Some(a.requested_at),
        updated_at: None,
    }
}

/// A pending Operative awaiting activation with no Clearance to decide (a
/// `route=direct` hire) — needs an explicit `agent.approve_hire`.
pub fn hire_item(p: &AgentProfile) -> ActionItem {
    ActionItem {
        id: format!("hire:{}", p.agent_id),
        category: ActionCategory::Hire,
        severity: ActionCategory::Hire.severity(),
        title: format!("Approve hire — {}", p.name),
        reason: format!("a pending {} hire is inert until approved", p.role),
        target_type: Some("agent".to_string()),
        target_id: Some(p.agent_id.clone()),
        target_title: Some(p.name.clone()),
        action_label: "Approve the hire".to_string(),
        route: Some("/agents".to_string()),
        created_at: Some(p.created_at),
        updated_at: None,
    }
}

/// Micro-USD per cent — the unit bridge between the metrics ledger's
/// `cost_micros` (what the dispatch gate sums) and an Allowance/budget expressed
/// in cents. Mirrors `heartbeat::MICROS_PER_CENT` (the gate's constant); kept
/// local so this PURE module carries no dependency on the dispatch layer.
pub const MICROS_PER_CENT: u64 = 10_000;

/// The "near" warning band for ACTUAL spend: a spend alert fires once spend
/// reaches this percentage of the cap/budget — strictly below the gate's 100%
/// hard refusal — so the operator gets runway before a hard-stop. The gate
/// itself only refuses at ≥ 100% (`heartbeat::allowance_admits`).
pub const SPEND_NEAR_PCT: u64 = 80;

/// Render a cents amount as `$D.CC` for an operator-facing reason line.
fn fmt_cents(cents: i64) -> String {
    let neg = cents < 0;
    let abs = cents.unsigned_abs();
    format!("{}${}.{:02}", if neg { "-" } else { "" }, abs / 100, abs % 100)
}

/// Render a micro-USD amount as `$D.CC` (the metrics ledger's `cost_micros`
/// unit; 1 USD = 1_000_000 micros). Truncates to the nearest whole cent for
/// display, so it never implies *more* spend than the ledger recorded.
fn fmt_micros(micros: u64) -> String {
    let cents = micros / MICROS_PER_CENT; // 10_000 micros = 1 cent (floor sub-cent)
    format!("${}.{:02}", cents / 100, cents % 100)
}

/// **Budget alert (Part A)** — the Guild's committed Allowance against its
/// configured budget (company-model §5.4 the Board reads/sets budgets, §8.2 the
/// Inbox surfaces budget thresholds). `committed` is the sum of active
/// Operatives' Allowances; `budget` is the Guild's monthly Allowance. Both come
/// from EXISTING tenant-scoped state — there is no fabricated live-spend figure.
/// `over = true` means committed already exceeds the budget (High); otherwise it
/// is *near* the budget (Medium). A single stable id so it never spams.
pub fn budget_committed_item(committed_cents: i64, budget_cents: i64, over: bool) -> ActionItem {
    let pct = if budget_cents > 0 {
        committed_cents.saturating_mul(100) / budget_cents
    } else {
        0
    };
    let (title, reason, severity) = if over {
        (
            "Committed Allowance over the Guild budget".to_string(),
            format!(
                "active Operatives commit {} of the {} Guild budget ({pct}%) — over budget; \
                 trim Allowances or raise the Guild budget",
                fmt_cents(committed_cents),
                fmt_cents(budget_cents),
            ),
            ActionSeverity::High,
        )
    } else {
        (
            "Committed Allowance near the Guild budget".to_string(),
            format!(
                "active Operatives commit {} of the {} Guild budget ({pct}%) — approaching the cap",
                fmt_cents(committed_cents),
                fmt_cents(budget_cents),
            ),
            ActionSeverity::Medium,
        )
    };
    ActionItem {
        id: "budget:committed".to_string(),
        category: ActionCategory::Budget,
        severity,
        title,
        reason,
        target_type: Some("company".to_string()),
        target_id: Some("committed_allowance".to_string()),
        target_title: None,
        action_label: "Review Allowances".to_string(),
        route: Some("/agents".to_string()),
        created_at: None,
        updated_at: None,
    }
}

/// **Budget alert (Part A)** — an active Operative whose Allowance is `0`/negative
/// is hard-stopped by the dispatch gate (`heartbeat::allowance_admits`: `c <= 0
/// ⇒ Refuse`). When such an Operative has runnable/blocked work assigned, surface
/// it so the Board raises the cap. Config-only and fully honest — no spend ledger.
pub fn allowance_hardstop_item(p: &AgentProfile) -> ActionItem {
    ActionItem {
        id: format!("budget:hardstop:{}", p.agent_id),
        category: ActionCategory::Budget,
        severity: ActionSeverity::High,
        title: format!("Allowance hard-stop — {}", p.name),
        reason:
            "this Operative's Allowance is 0 (hard-stopped) but it has runnable or blocked work \
             assigned — raise the Allowance so it can run"
                .to_string(),
        target_type: Some("agent".to_string()),
        target_id: Some(p.agent_id.clone()),
        target_title: Some(p.name.clone()),
        action_label: "Raise the Allowance".to_string(),
        route: Some("/agents".to_string()),
        created_at: Some(p.created_at),
        updated_at: None,
    }
}

/// A read-only view of authoritative **month-to-date spend** — the SAME source
/// and window the dispatch/refusal gate enforces (`MetricsQuery::cost_since`
/// summing `cost_micros` over the trailing 30 days; see the heartbeat Allowance
/// gate in `controller_runtime` and `heartbeat::allowance_admits`).
///
/// The Action Center reads spend ONLY through this seam so that:
/// - it can never fabricate a spend figure — the production impl is backed by
///   the metrics ledger, and when metrics are disabled the handler is handed
///   `None` and emits NO spend item at all;
/// - it stays unit-testable with a fake that has no SQLite dependency;
/// - it is tenant-safe by construction — the handler only ever asks about
///   Operative ids it already resolved from the caller's OWN tenant roster, so
///   no cross-tenant or company-wide (`cost_since(None, …)`) total is ever read.
pub trait SpendSource {
    /// Trailing-30-day spend for ONE Operative, in micro-USD. `None` means the
    /// ledger could not answer (treated as "no spend signal" — never silently
    /// as `0`, so a transient read failure can't fabricate or suppress an alert
    /// dishonestly).
    fn operative_spend_micros(&self, agent_id: &str) -> Option<u64>;
}

/// **Live spend alert (Part B)** — an active Operative's ACTUAL trailing-30-day
/// spend against its configured Allowance, read from the SAME metrics ledger +
/// window the dispatch gate enforces (`MetricsQuery::cost_since`). `over = true`
/// means spend has reached/passed the cap — the gate's `over_allowance` refusal
/// threshold (High); otherwise it is *near* the cap (≥ [`SPEND_NEAR_PCT`] —
/// Medium). DISTINCT from the committed-Allowance planning signal
/// ([`budget_committed_item`]): this is money already *spent*, not capacity
/// *reserved*. `spend_micros` is micro-USD; `cap_cents` is the Allowance.
pub fn operative_spend_item(
    p: &AgentProfile,
    spend_micros: u64,
    cap_cents: i64,
    over: bool,
) -> ActionItem {
    let cap_micros = (cap_cents.max(0) as u64).saturating_mul(MICROS_PER_CENT);
    let pct = spend_micros
        .saturating_mul(100)
        .checked_div(cap_micros)
        .unwrap_or(0);
    let (title, reason, severity) = if over {
        (
            format!("Spend over Allowance — {}", p.name),
            format!(
                "spent {} of the {} monthly Allowance ({pct}%) in the last 30 days — at/over the cap; \
                 the dispatch gate now refuses this Operative. Raise the Allowance to resume",
                fmt_micros(spend_micros),
                fmt_cents(cap_cents),
            ),
            ActionSeverity::High,
        )
    } else {
        (
            format!("Spend near Allowance — {}", p.name),
            format!(
                "spent {} of the {} monthly Allowance ({pct}%) in the last 30 days — approaching the cap",
                fmt_micros(spend_micros),
                fmt_cents(cap_cents),
            ),
            ActionSeverity::Medium,
        )
    };
    ActionItem {
        id: format!("budget:spend:{}", p.agent_id),
        category: ActionCategory::Budget,
        severity,
        title,
        reason,
        target_type: Some("agent".to_string()),
        target_id: Some(p.agent_id.clone()),
        target_title: Some(p.name.clone()),
        action_label: "Review the Allowance".to_string(),
        route: Some("/agents".to_string()),
        created_at: Some(p.created_at),
        updated_at: None,
    }
}

/// **Live spend alert (Part B)** — the Guild's ACTUAL trailing-30-day spend (the
/// sum of THIS tenant's active Operatives' `cost_since`, never a cross-tenant
/// `cost_since(None, …)`) against its configured monthly budget. `over = true`
/// means actual spend has reached/passed the budget (High); otherwise it is
/// *near* it (≥ [`SPEND_NEAR_PCT`] — Medium). DISTINCT from the committed-
/// Allowance item: committed is capacity *reserved*; this is money already
/// *spent*. A single stable id so it never spams. `spend_micros` is micro-USD;
/// `budget_cents` is the Guild budget.
pub fn company_spend_item(spend_micros: u64, budget_cents: i64, over: bool) -> ActionItem {
    let budget_micros = (budget_cents.max(0) as u64).saturating_mul(MICROS_PER_CENT);
    let pct = spend_micros
        .saturating_mul(100)
        .checked_div(budget_micros)
        .unwrap_or(0);
    let (title, reason, severity) = if over {
        (
            "Guild spend over budget".to_string(),
            format!(
                "the Guild has spent {} of its {} monthly budget ({pct}%) in the last 30 days — over \
                 budget; raise the budget or trim Operative spend",
                fmt_micros(spend_micros),
                fmt_cents(budget_cents),
            ),
            ActionSeverity::High,
        )
    } else {
        (
            "Guild spend near budget".to_string(),
            format!(
                "the Guild has spent {} of its {} monthly budget ({pct}%) in the last 30 days — \
                 approaching the cap",
                fmt_micros(spend_micros),
                fmt_cents(budget_cents),
            ),
            ActionSeverity::Medium,
        )
    };
    ActionItem {
        id: "budget:spend:company".to_string(),
        category: ActionCategory::Budget,
        severity,
        title,
        reason,
        target_type: Some("company".to_string()),
        target_id: Some("actual_spend".to_string()),
        target_title: None,
        action_label: "Review spend".to_string(),
        route: Some("/agents".to_string()),
        created_at: None,
        updated_at: None,
    }
}

/// A Mandate whose strategy is `proposed` and awaits the Board's approval
/// (company-model §5.5 strategy gate).
pub fn strategy_item(m: &Mandate) -> ActionItem {
    ActionItem {
        id: format!("strategy:{}", m.mandate_id),
        category: ActionCategory::Approval,
        severity: ActionCategory::Approval.severity(),
        title: format!("Approve strategy — {}", snippet(&m.title)),
        reason: "the Mandate strategy is proposed and must be approved before the team can be built"
            .to_string(),
        target_type: Some("mandate".to_string()),
        target_id: Some(m.mandate_id.clone()),
        target_title: Some(snippet(&m.title)),
        action_label: "Approve the strategy".to_string(),
        route: Some("/mandates".to_string()),
        created_at: Some(m.created_at),
        updated_at: Some(m.updated_at),
    }
}

/// A Brief ready to become a Shift right now.
pub fn ready_item(c: &BriefCard) -> ActionItem {
    ActionItem {
        id: format!("ready:{}", c.task_id),
        category: ActionCategory::ReadyToStart,
        severity: ActionCategory::ReadyToStart.severity(),
        title: format!("Start: {}", snippet(&c.title)),
        reason: "assigned to an active Operative and unblocked — ready to run".to_string(),
        target_type: Some("brief".to_string()),
        target_id: Some(c.task_id.clone()),
        target_title: Some(snippet(&c.title)),
        action_label: "Start the Brief".to_string(),
        route: Some("/briefs".to_string()),
        created_at: None,
        updated_at: None,
    }
}

/// A Brief that cannot start: blocked on a dependency, or missing an assignee.
/// `unassigned` distinguishes the two so the reason + action are honest.
pub fn blocked_item(c: &BriefCard, unassigned: bool) -> ActionItem {
    let (reason, action_label, route) = if unassigned {
        (
            "no Operative assigned — assign one (or approve a hire) so it can run".to_string(),
            "Assign an Operative".to_string(),
            Some("/briefs".to_string()),
        )
    } else {
        (
            "blocked on a dependency Brief — resolve the blocker".to_string(),
            "Resolve the blocker".to_string(),
            Some("/briefs".to_string()),
        )
    };
    ActionItem {
        id: format!("blocked:{}", c.task_id),
        category: ActionCategory::Blocked,
        severity: ActionCategory::Blocked.severity(),
        title: format!("Blocked: {}", snippet(&c.title)),
        reason,
        target_type: Some("brief".to_string()),
        target_id: Some(c.task_id.clone()),
        target_title: Some(snippet(&c.title)),
        action_label,
        route,
        created_at: None,
        updated_at: None,
    }
}

/// A Brief that has sat too long in an active column with nobody moving it.
pub fn stale_item(c: &BriefCard) -> ActionItem {
    ActionItem {
        id: format!("stale:{}", c.task_id),
        category: ActionCategory::Stale,
        severity: ActionCategory::Stale.severity(),
        title: format!("Stale: {}", snippet(&c.title)),
        reason: format!(
            "stuck in `{}` with no recent progress — nudge, reassign, or close it",
            c.board_status
        ),
        target_type: Some("brief".to_string()),
        target_id: Some(c.task_id.clone()),
        target_title: Some(snippet(&c.title)),
        action_label: "Review the stalled Brief".to_string(),
        route: Some("/briefs".to_string()),
        created_at: None,
        updated_at: None,
    }
}

/// A completed Shift awaiting review (`done` + `pending_review`). Targets the
/// Brief (so it dedupes against other Brief items) but deep-links to the run.
pub fn needs_review_item(r: &RunRecord) -> ActionItem {
    ActionItem {
        id: format!("review:{}", r.run_id),
        category: ActionCategory::NeedsReview,
        severity: ActionCategory::NeedsReview.severity(),
        title: "Review a completed Shift".to_string(),
        reason: format!("a {} Shift finished and awaits review → apply", r.rig),
        target_type: Some("brief".to_string()),
        target_id: Some(r.brief_id.clone()),
        target_title: None,
        action_label: "Review the run".to_string(),
        route: Some(format!("/runs?run={}", r.run_id)),
        created_at: Some(r.started_at),
        updated_at: r.finished_at,
    }
}

/// Map a terminal run state + durable refusal reason to a plain-language root
/// cause and a **recommended recovery action + route** — the read-only form of
/// the Inbox recovery-decision card (dashboard-design §5.2 / execution §3.3b).
///
/// HONEST SCOPE: there is no diagnosis layer and no failure-class/retry-budget
/// on a run (only the existing **durable refusal taxonomy** —
/// `unassigned` / `no_adapter` / `adapter_unavailable` / `over_allowance` /
/// `workspace_error` / `workspace_context_error`, see `refusal_is_durable`). Each
/// known cause maps to the EXISTING governed route that fixes it; we mint no new
/// retry/apply mutation. Returns `(reason, action_label, route)`.
fn recovery_reco(r: &RunRecord) -> (String, String, String) {
    let run_route = format!("/runs?run={}", r.run_id);
    match r.status.as_str() {
        "refused" => match r.refusal_reason.as_deref().unwrap_or("") {
            "unassigned" => (
                "the Shift was refused — no Operative is assigned to this Brief".to_string(),
                "Assign an Operative".to_string(),
                "/briefs".to_string(),
            ),
            "no_adapter" => (
                "the Shift was refused — the configured Rig is not installed".to_string(),
                "Configure the Rig".to_string(),
                "/settings".to_string(),
            ),
            "adapter_unavailable" => (
                "the Shift was refused — the Rig is installed but not authenticated".to_string(),
                "Configure the Rig".to_string(),
                "/settings".to_string(),
            ),
            "over_allowance" => (
                "the Shift was refused — the Operative is over its monthly Allowance".to_string(),
                "Raise the Allowance".to_string(),
                "/agents".to_string(),
            ),
            "workspace_error" | "workspace_context_error" => (
                "the Shift was refused — the run workspace could not be prepared".to_string(),
                "Review runtime settings".to_string(),
                "/settings".to_string(),
            ),
            other if !other.is_empty() => (
                format!("the Shift was refused: {other}"),
                "Inspect the run".to_string(),
                run_route,
            ),
            _ => (
                "the Shift was refused before it ran".to_string(),
                "Inspect the run".to_string(),
                run_route,
            ),
        },
        "interrupted" => (
            "the Shift was interrupted (the executing process is gone) — it will be re-claimed \
             automatically; inspect if it recurs"
                .to_string(),
            "Inspect the run".to_string(),
            run_route,
        ),
        // "failed" (or any other non-refused terminal): the Rig ran and failed,
        // so the (already secret-redacted) summary is the best available reason.
        _ => {
            let why = snippet(&r.summary);
            let reason = if why.trim().is_empty() {
                format!("a Shift ended `{}` and needs attention", r.status)
            } else {
                why
            };
            (reason, "Inspect the run".to_string(), run_route)
        }
    }
}

/// A Shift that failed / was refused / was interrupted and needs operator
/// attention — a read-only recovery-decision card (Part B). The reason states
/// the root cause and the action/route points at the EXISTING governed fix
/// (assign · configure Rig · raise Allowance · review runtime · inspect).
pub fn failed_item(r: &RunRecord) -> ActionItem {
    let (reason, action_label, route) = recovery_reco(r);
    ActionItem {
        id: format!("failed:{}", r.run_id),
        category: ActionCategory::FailedOrRefused,
        severity: ActionCategory::FailedOrRefused.severity(),
        title: format!("Shift {} — {}", r.status, r.rig),
        reason,
        target_type: Some("brief".to_string()),
        target_id: Some(r.brief_id.clone()),
        target_title: None,
        action_label,
        route: Some(route),
        created_at: Some(r.started_at),
        updated_at: r.finished_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, cat: ActionCategory, target: Option<(&str, &str)>, created: Option<i64>) -> ActionItem {
        ActionItem {
            id: id.to_string(),
            category: cat,
            severity: cat.severity(),
            title: id.to_string(),
            reason: String::new(),
            target_type: target.map(|(t, _)| t.to_string()),
            target_id: target.map(|(_, i)| i.to_string()),
            target_title: None,
            action_label: String::new(),
            route: None,
            created_at: created,
            updated_at: None,
        }
    }

    #[test]
    fn ranks_are_strictly_ordered_high_to_low() {
        // The pack's required ordering: approvals/hire near the top, failed
        // before stale, ready_to_start before blocked.
        let order = [
            ActionCategory::Approval,
            ActionCategory::Hire,
            ActionCategory::Budget,
            ActionCategory::FailedOrRefused,
            ActionCategory::NeedsReview,
            ActionCategory::ReadyToStart,
            ActionCategory::Blocked,
            ActionCategory::Stale,
        ];
        for w in order.windows(2) {
            assert!(w[0].rank() < w[1].rank(), "{:?} must rank before {:?}", w[0], w[1]);
        }
        // The specific guarantees called out in the brief:
        assert!(ActionCategory::FailedOrRefused.rank() < ActionCategory::Stale.rank());
        assert!(ActionCategory::ReadyToStart.rank() < ActionCategory::Blocked.rank());
        assert!(ActionCategory::Approval.rank() < ActionCategory::FailedOrRefused.rank());
        // Budget governance sits with the approval/hire blockers, above recovery.
        assert!(ActionCategory::Hire.rank() < ActionCategory::Budget.rank());
        assert!(ActionCategory::Budget.rank() < ActionCategory::FailedOrRefused.rank());
    }

    #[test]
    fn severity_maps_high_medium_low() {
        assert_eq!(ActionCategory::Approval.severity(), ActionSeverity::High);
        assert_eq!(ActionCategory::Budget.severity(), ActionSeverity::High);
        assert_eq!(ActionCategory::FailedOrRefused.severity(), ActionSeverity::High);
        assert_eq!(ActionCategory::ReadyToStart.severity(), ActionSeverity::Medium);
        assert_eq!(ActionCategory::Stale.severity(), ActionSeverity::Low);
    }

    #[test]
    fn fmt_cents_renders_dollars() {
        assert_eq!(fmt_cents(0), "$0.00");
        assert_eq!(fmt_cents(5), "$0.05");
        assert_eq!(fmt_cents(12_345), "$123.45");
        assert_eq!(fmt_cents(-200), "-$2.00");
    }

    #[test]
    fn budget_committed_over_vs_near_severity_and_dedupe_key() {
        let over = budget_committed_item(15_000, 10_000, true);
        assert_eq!(over.category, ActionCategory::Budget);
        assert_eq!(over.severity, ActionSeverity::High);
        assert!(over.reason.contains("over budget"));
        assert!(over.reason.contains("$150.00"));
        assert!(over.reason.contains("150%"));

        let near = budget_committed_item(9_500, 10_000, false);
        assert_eq!(near.severity, ActionSeverity::Medium);
        assert!(near.reason.contains("approaching"));
        // Both share the singleton id/target so only one can ever survive.
        assert_eq!(over.id, near.id);
        assert_eq!(over.dedupe_key(), near.dedupe_key());
    }

    #[test]
    fn fmt_micros_renders_dollars() {
        assert_eq!(fmt_micros(0), "$0.00");
        assert_eq!(fmt_micros(10_000), "$0.01"); // 1 cent
        assert_eq!(fmt_micros(1_000_000), "$1.00"); // 1 USD
        assert_eq!(fmt_micros(123_450_000), "$123.45");
        // Sub-cent is floored, never rounded up (no over-statement of spend).
        assert_eq!(fmt_micros(19_999), "$0.01");
    }

    #[test]
    fn company_spend_over_vs_near_severity_distinct_from_committed() {
        // $150 spent of a $100 budget → over (High). budget_cents=10_000 ($100),
        // spend_micros=150_000_000 ($150).
        let over = company_spend_item(150_000_000, 10_000, true);
        assert_eq!(over.category, ActionCategory::Budget);
        assert_eq!(over.severity, ActionSeverity::High);
        assert!(over.reason.contains("over budget"));
        assert!(over.reason.contains("$150.00"));
        assert!(over.reason.contains("150%"));
        assert!(over.reason.contains("30 days"), "window stated: {}", over.reason);

        // $85 spent of a $100 budget → near (Medium).
        let near = company_spend_item(85_000_000, 10_000, false);
        assert_eq!(near.severity, ActionSeverity::Medium);
        assert!(near.reason.contains("approaching"));
        assert!(near.reason.contains("85%"));

        // The ACTUAL-spend company item is a SEPARATE object from the
        // committed-Allowance planning item: distinct id + dedupe key, so both
        // can coexist in the feed without collapsing onto one another.
        let committed = budget_committed_item(9_000, 10_000, false);
        assert_ne!(over.id, committed.id);
        assert_ne!(over.dedupe_key(), committed.dedupe_key());
    }

    fn run(status: &str, refusal: Option<&str>, summary: &str) -> RunRecord {
        RunRecord {
            run_id: "run-1".into(),
            brief_id: "brief-1".into(),
            agent_id: "agt-1".into(),
            rig: "claude".into(),
            status: status.into(),
            started_at: 100,
            finished_at: Some(200),
            duration_secs: Some(100),
            summary: summary.into(),
            workspace: None,
            workspace_context: None,
            workspace_files: None,
            workspace_bytes: None,
            review: None,
            review_note: None,
            reviewed_at: None,
            apply_status: None,
            applied_at: None,
            apply_note: None,
            applied_files: None,
            failed_files: None,
            trigger: Some("manual".into()),
            provider: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            cached_input_tokens: None,
            cost_micros: None,
            session_id: None,
            refusal_reason: refusal.map(|s| s.to_string()),
        }
    }

    #[test]
    fn recovery_card_maps_refusal_to_stable_action_and_route() {
        // Each durable refusal reason → a STABLE recommended action + the
        // existing governed route that fixes it.
        let cases = [
            ("unassigned", "Assign an Operative", "/briefs"),
            ("no_adapter", "Configure the Rig", "/settings"),
            ("adapter_unavailable", "Configure the Rig", "/settings"),
            ("over_allowance", "Raise the Allowance", "/agents"),
            ("workspace_error", "Review runtime settings", "/settings"),
            ("workspace_context_error", "Review runtime settings", "/settings"),
        ];
        for (reason_code, label, route) in cases {
            let it = failed_item(&run("refused", Some(reason_code), ""));
            assert_eq!(it.category, ActionCategory::FailedOrRefused);
            assert_eq!(it.action_label, label, "label for {reason_code}");
            assert_eq!(it.route.as_deref(), Some(route), "route for {reason_code}");
            assert!(!it.reason.trim().is_empty(), "reason for {reason_code}");
        }
    }

    #[test]
    fn recovery_card_failed_carries_summary_reason_and_inspect() {
        let it = failed_item(&run("failed", None, "compiler error: E0277"));
        assert_eq!(it.action_label, "Inspect the run");
        assert_eq!(it.route.as_deref(), Some("/runs?run=run-1"));
        assert!(it.reason.contains("E0277"));

        // Interrupted explains auto-reclaim + inspect.
        let it = failed_item(&run("interrupted", None, ""));
        assert!(it.reason.contains("interrupted"));
        assert_eq!(it.action_label, "Inspect the run");

        // Unknown refusal reason still produces a non-empty reason + inspect.
        let it = failed_item(&run("refused", Some("mystery"), ""));
        assert!(it.reason.contains("mystery"));
        assert_eq!(it.action_label, "Inspect the run");
    }

    #[test]
    fn finalize_orders_by_rank_then_oldest_first() {
        let items = vec![
            item("stale:1", ActionCategory::Stale, Some(("brief", "s1")), Some(10)),
            item("ready:1", ActionCategory::ReadyToStart, Some(("brief", "r1")), Some(10)),
            item("approval:new", ActionCategory::Approval, Some(("agent", "a2")), Some(200)),
            item("approval:old", ActionCategory::Approval, Some(("agent", "a1")), Some(100)),
        ];
        let out = finalize(items);
        let ids: Vec<&str> = out.iter().map(|i| i.id.as_str()).collect();
        // approvals first (oldest before newer), then ready, then stale.
        assert_eq!(ids, ["approval:old", "approval:new", "ready:1", "stale:1"]);
    }

    #[test]
    fn finalize_dedupes_same_object_keeping_most_urgent() {
        // A pending hire (rank 1) AND its spawn Clearance/approval (rank 0)
        // both target the same agent → only the approval survives.
        let items = vec![
            item("hire:x", ActionCategory::Hire, Some(("agent", "agt-x")), Some(50)),
            item("approval:x", ActionCategory::Approval, Some(("agent", "agt-x")), Some(40)),
        ];
        let out = finalize(items);
        assert_eq!(out.len(), 1, "the same object must not spam the operator");
        assert_eq!(out[0].id, "approval:x");
        assert_eq!(out[0].category, ActionCategory::Approval);
    }

    #[test]
    fn finalize_dedupes_brief_across_categories() {
        // A Brief that is both failed (rank 2) and stale (rank 6) → failed wins.
        let items = vec![
            item("stale:b", ActionCategory::Stale, Some(("brief", "b1")), Some(5)),
            item("failed:b", ActionCategory::FailedOrRefused, Some(("brief", "b1")), Some(9)),
        ];
        let out = finalize(items);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].category, ActionCategory::FailedOrRefused);
    }

    #[test]
    fn finalize_never_dedupes_targetless_items() {
        let items = vec![
            item("a", ActionCategory::Approval, None, Some(1)),
            item("b", ActionCategory::Approval, None, Some(2)),
        ];
        assert_eq!(finalize(items).len(), 2);
    }

    #[test]
    fn finalize_is_deterministic_on_id_tiebreak() {
        // Same rank + same created_at → stable order by id.
        let a = item("ready:z", ActionCategory::ReadyToStart, Some(("brief", "z")), Some(1));
        let b = item("ready:a", ActionCategory::ReadyToStart, Some(("brief", "a")), Some(1));
        let out = finalize(vec![a, b]);
        assert_eq!(out[0].id, "ready:a");
        assert_eq!(out[1].id, "ready:z");
    }
}
