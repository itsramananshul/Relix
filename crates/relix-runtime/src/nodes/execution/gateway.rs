//! Transactional action gateway.
//!
//! When a sequence of tool calls runs as part of one
//! `ai.chat` plan, the operator wants two guarantees:
//!
//! 1. **Visibility of partial progress** — when step N fails
//!    after steps 1..N-1 succeeded, the operator should see
//!    *which* succeeded so they can decide whether to roll
//!    them back manually.
//! 2. **Loud rollback signal when irreversible actions
//!    completed pre-failure** — if step 1 sent an email and
//!    step 2 failed, the operator must be notified, not have
//!    the email silently disappear from the audit.
//!
//! `ActionGateway` records every completed / failed action in
//! order and provides two render helpers: a rollback
//! notification (operator-readable, only fires when
//! irreversible actions completed before a failure) and a
//! transaction summary (everything, success or not, for the
//! chronicle).
//!
//! The gateway is pure data + render — it doesn't dispatch
//! tools. Callers feed in results as they happen.

use serde::Serialize;

/// One action recorded by the gateway. `result` is `Some`
/// for completed actions (the tool's reply string) and
/// `None` for actions that failed before producing a reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GatewayAction {
    pub tool: String,
    pub args: String,
    pub result: Option<String>,
    pub reversible: bool,
    /// Optional operator-facing hint for how to undo the
    /// action ("re-fetch and re-send" / "manually retract
    /// the email" / ...). Used by the rollback-notification
    /// renderer.
    pub rollback_hint: Option<String>,
}

impl GatewayAction {
    /// Convenience constructor: empty result, no hint.
    pub fn new(tool: impl Into<String>, args: impl Into<String>, reversible: bool) -> Self {
        Self {
            tool: tool.into(),
            args: args.into(),
            result: None,
            reversible,
            rollback_hint: None,
        }
    }

    pub fn with_result(mut self, result: impl Into<String>) -> Self {
        self.result = Some(result.into());
        self
    }

    pub fn with_rollback_hint(mut self, hint: impl Into<String>) -> Self {
        self.rollback_hint = Some(hint.into());
        self
    }
}

/// Tracks one sequence of tool calls.
#[derive(Clone, Debug, Default)]
pub struct ActionGateway {
    completed: Vec<GatewayAction>,
    failed: Vec<GatewayAction>,
}

impl ActionGateway {
    pub fn new() -> Self {
        Self {
            completed: Vec::new(),
            failed: Vec::new(),
        }
    }

    pub fn record_completed(&mut self, action: GatewayAction) {
        self.completed.push(action);
    }

    pub fn record_failed(&mut self, action: GatewayAction) {
        self.failed.push(action);
    }

    /// `true` when **any** irreversible action completed
    /// successfully AND any action failed afterwards. That's
    /// the signal that the operator needs to see — completed
    /// reversible actions are fine to skip silently (the
    /// runtime can undo them on the next plan).
    pub fn needs_rollback_notification(&self) -> bool {
        if self.failed.is_empty() {
            return false;
        }
        self.completed.iter().any(|a| !a.reversible)
    }

    /// Operator-facing notification listing every
    /// irreversible action that completed before the
    /// failure. When no irreversible work landed, returns an
    /// empty string so callers can pair this with
    /// `needs_rollback_notification`.
    pub fn rollback_notification(&self) -> String {
        if !self.needs_rollback_notification() {
            return String::new();
        }
        let mut out = String::new();
        out.push_str("ROLLBACK NEEDED — the following irreversible actions completed before a step failed:\n");
        for a in self.completed.iter().filter(|a| !a.reversible) {
            let hint = a
                .rollback_hint
                .as_deref()
                .unwrap_or("no rollback hint provided");
            out.push_str(&format!(
                "- {} (args: {}) — rollback hint: {hint}\n",
                a.tool,
                preview(&a.args, 120)
            ));
        }
        out.push_str("Failed steps:\n");
        for a in &self.failed {
            out.push_str(&format!("- {} (args: {})\n", a.tool, preview(&a.args, 120)));
        }
        out
    }

    /// Chronicle-shaped summary of the entire transaction.
    /// Always non-empty; lists every completed + failed
    /// action so the operator can read the full picture from
    /// one entry.
    pub fn transaction_summary(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "actions: completed={} failed={}\n",
            self.completed.len(),
            self.failed.len()
        ));
        for a in &self.completed {
            let result_preview = a
                .result
                .as_deref()
                .map(|r| preview(r, 80))
                .unwrap_or_else(|| "(no result captured)".to_string());
            let marker = if a.reversible { "rev" } else { "IRREVERSIBLE" };
            out.push_str(&format!("OK   [{marker}] {} -> {result_preview}\n", a.tool));
        }
        for a in &self.failed {
            let marker = if a.reversible { "rev" } else { "irrev" };
            out.push_str(&format!("FAIL [{marker}] {}\n", a.tool));
        }
        out
    }

    /// Counts useful for tests + the dashboard summary.
    pub fn completed_count(&self) -> usize {
        self.completed.len()
    }

    pub fn failed_count(&self) -> usize {
        self.failed.len()
    }

    pub fn completed(&self) -> &[GatewayAction] {
        &self.completed
    }

    pub fn failed(&self) -> &[GatewayAction] {
        &self.failed
    }
}

fn preview(s: &str, max: usize) -> String {
    let trimmed: String = s.chars().take(max).collect();
    if trimmed.chars().count() < s.chars().count() {
        format!("{trimmed}…")
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rev_action(tool: &str) -> GatewayAction {
        GatewayAction::new(tool, "args", true).with_result("ok")
    }

    fn irrev_action(tool: &str) -> GatewayAction {
        GatewayAction::new(tool, "args", false)
            .with_result("sent")
            .with_rollback_hint(format!("manually retract {tool}"))
    }

    #[test]
    fn record_completed_tracks_in_order() {
        let mut g = ActionGateway::new();
        g.record_completed(rev_action("a.fetch"));
        g.record_completed(rev_action("b.fetch"));
        assert_eq!(g.completed_count(), 2);
        assert_eq!(g.failed_count(), 0);
        assert_eq!(g.completed()[0].tool, "a.fetch");
        assert_eq!(g.completed()[1].tool, "b.fetch");
    }

    #[test]
    fn record_failed_with_irreversible_predecessors_signals_rollback() {
        let mut g = ActionGateway::new();
        g.record_completed(irrev_action("email.send"));
        g.record_failed(GatewayAction::new("db.commit", "x", true));
        assert!(g.needs_rollback_notification());
        let notice = g.rollback_notification();
        assert!(notice.starts_with("ROLLBACK NEEDED"));
        assert!(notice.contains("email.send"));
        assert!(notice.contains("manually retract email.send"));
        // The failed step is also listed for context.
        assert!(notice.contains("db.commit"));
    }

    #[test]
    fn rollback_notification_lists_only_irreversible_completed() {
        let mut g = ActionGateway::new();
        g.record_completed(rev_action("web.fetch"));
        g.record_completed(irrev_action("email.send"));
        g.record_completed(rev_action("memory.search"));
        g.record_failed(GatewayAction::new("payment.charge", "x", false));
        let notice = g.rollback_notification();
        assert!(notice.contains("email.send"));
        // Reversible completed actions are intentionally not
        // listed — the runtime can undo them on the next
        // plan without operator intervention.
        assert!(!notice.contains("web.fetch"));
        assert!(!notice.contains("memory.search"));
    }

    #[test]
    fn transaction_summary_includes_all_actions() {
        let mut g = ActionGateway::new();
        g.record_completed(rev_action("a.fetch"));
        g.record_completed(irrev_action("email.send"));
        g.record_failed(GatewayAction::new("db.commit", "x", true));
        let summary = g.transaction_summary();
        // Header counts.
        assert!(summary.contains("completed=2 failed=1"));
        // Reversibility annotations.
        assert!(summary.contains("OK   [rev] a.fetch"));
        assert!(summary.contains("OK   [IRREVERSIBLE] email.send"));
        assert!(summary.contains("FAIL [rev] db.commit"));
    }

    #[test]
    fn no_rollback_notification_when_all_completed_are_reversible() {
        let mut g = ActionGateway::new();
        g.record_completed(rev_action("a.fetch"));
        g.record_completed(rev_action("b.fetch"));
        g.record_failed(GatewayAction::new("c.write", "x", false));
        assert!(!g.needs_rollback_notification());
        assert!(g.rollback_notification().is_empty());
    }

    #[test]
    fn no_rollback_notification_when_no_failures() {
        let mut g = ActionGateway::new();
        g.record_completed(irrev_action("email.send"));
        g.record_completed(rev_action("memory.search"));
        // All succeeded — nothing to roll back.
        assert!(!g.needs_rollback_notification());
    }

    #[test]
    fn transaction_summary_truncates_long_results() {
        let mut g = ActionGateway::new();
        let big = "x".repeat(500);
        g.record_completed(GatewayAction::new("web.fetch", "url", true).with_result(big));
        let summary = g.transaction_summary();
        assert!(summary.contains("…"));
        // The line is bounded — the 500-char result doesn't
        // appear in full.
        assert!(summary.len() < 500);
    }

    #[test]
    fn empty_gateway_renders_empty_state_cleanly() {
        let g = ActionGateway::new();
        assert!(!g.needs_rollback_notification());
        assert!(g.rollback_notification().is_empty());
        let summary = g.transaction_summary();
        assert!(summary.contains("completed=0 failed=0"));
    }
}
