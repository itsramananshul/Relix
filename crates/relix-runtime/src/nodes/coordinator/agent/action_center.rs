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
    /// - failed/refused before informational stale items,
    /// - ready_to_start before generic blocked items (it can move work forward).
    pub fn rank(self) -> u8 {
        match self {
            ActionCategory::Approval => 0,
            ActionCategory::Hire => 1,
            ActionCategory::FailedOrRefused => 2,
            ActionCategory::NeedsReview => 3,
            ActionCategory::ReadyToStart => 4,
            ActionCategory::Blocked => 5,
            ActionCategory::Stale => 6,
        }
    }

    /// The coarse severity badge for the dashboard.
    pub fn severity(self) -> ActionSeverity {
        match self {
            ActionCategory::Approval | ActionCategory::Hire | ActionCategory::FailedOrRefused => {
                ActionSeverity::High
            }
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

/// A Shift that failed / was refused / was interrupted and needs operator
/// attention. `status` is the run's terminal state.
pub fn failed_item(r: &RunRecord) -> ActionItem {
    let why = match r.status.as_str() {
        "refused" => r
            .refusal_reason
            .clone()
            .map(|x| format!("refused: {x}"))
            .unwrap_or_else(|| "the Shift was refused before it ran".to_string()),
        "interrupted" => "the Shift was interrupted (the executing process is gone)".to_string(),
        _ => snippet(&r.summary),
    };
    ActionItem {
        id: format!("failed:{}", r.run_id),
        category: ActionCategory::FailedOrRefused,
        severity: ActionCategory::FailedOrRefused.severity(),
        title: format!("Shift {} — {}", r.status, r.rig),
        reason: if why.trim().is_empty() {
            format!("a Shift ended `{}` and needs attention", r.status)
        } else {
            why
        },
        target_type: Some("brief".to_string()),
        target_id: Some(r.brief_id.clone()),
        target_title: None,
        action_label: "Inspect the run".to_string(),
        route: Some(format!("/runs?run={}", r.run_id)),
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
    }

    #[test]
    fn severity_maps_high_medium_low() {
        assert_eq!(ActionCategory::Approval.severity(), ActionSeverity::High);
        assert_eq!(ActionCategory::FailedOrRefused.severity(), ActionSeverity::High);
        assert_eq!(ActionCategory::ReadyToStart.severity(), ActionSeverity::Medium);
        assert_eq!(ActionCategory::Stale.severity(), ActionSeverity::Low);
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
