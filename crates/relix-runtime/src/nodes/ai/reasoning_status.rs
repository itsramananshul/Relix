//! RELIX-7.29 PART 5 — `reasoning.status` coordinator cap.
//!
//! Returns a single JSON snapshot of every §7.29 component's
//! configured-or-not state. The cap is always registered;
//! components that haven't been wired report `enabled: false`
//! with the stat counters showing zero so operators can tell
//! "off" apart from "broken".
//!
//! Surface:
//!
//! ```json
//! {
//!   "routing":         { "enabled": true,  "tiers": { ... } },
//!   "self_consistency":{ "enabled": false, "config": { ... }, "stats": { ... } },
//!   "belief_state":    { "enabled": true,  "config": { ... }, "tracked_sessions": 7 },
//!   "judge":           { "enabled": false, "config": { ... }, "stats": { ... } }
//! }
//! ```

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

use super::belief_state::BeliefStateTracker;
use super::judge::JudgeRecorder;
use super::tier_routing::TierRouter;

/// Shared snapshot of every §7.29 component's runtime state.
/// Cheap to clone (just Arcs).
#[derive(Clone)]
pub struct ReasoningStatus {
    routing: TierRouter,
    sc_cfg: crate::confidence::SelfConsistencyConfig,
    sc_stats: crate::confidence::SelfConsistencyStats,
    belief: BeliefStateTracker,
    judge_cfg: super::judge::JudgeConfig,
    judge_recorder: JudgeRecorder,
}

impl ReasoningStatus {
    pub fn new(
        routing: TierRouter,
        sc_cfg: crate::confidence::SelfConsistencyConfig,
        sc_stats: crate::confidence::SelfConsistencyStats,
        belief: BeliefStateTracker,
        judge_cfg: super::judge::JudgeConfig,
        judge_recorder: JudgeRecorder,
    ) -> Self {
        Self {
            routing,
            sc_cfg,
            sc_stats,
            belief,
            judge_cfg,
            judge_recorder,
        }
    }

    /// Build the JSON snapshot the cap returns.
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "routing": {
                "enabled": self.routing.enabled(),
                "config": self.routing.config(),
            },
            "self_consistency": {
                "enabled": self.sc_cfg.enabled,
                "config": &self.sc_cfg,
                "stats": self.sc_stats.snapshot(),
            },
            "belief_state": {
                "enabled": self.belief.enabled(),
                "config": self.belief.config(),
                "tracked_sessions": self.belief.len(),
            },
            "judge": {
                "enabled": self.judge_cfg.enabled,
                "config": &self.judge_cfg,
                "stats": self.judge_recorder.stats(),
            },
        })
    }
}

/// Wire `reasoning.status` onto `bridge`.
pub fn register(bridge: &mut DispatchBridge, status: ReasoningStatus) {
    bridge.register(
        "reasoning.status",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| {
            let status = status.clone();
            async move {
                let body = status.snapshot();
                match serde_json::to_vec(&body) {
                    Ok(b) => HandlerOutcome::Ok(b),
                    Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::RESPONDER_INTERNAL,
                        cause: format!("reasoning.status: encode: {e}"),
                        retry_hint: 0,
                        retry_after: None,
                    }),
                }
            }
        })),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confidence::{SelfConsistencyConfig, SelfConsistencyStats};

    fn fresh_status() -> ReasoningStatus {
        ReasoningStatus::new(
            TierRouter::default(),
            SelfConsistencyConfig::default(),
            SelfConsistencyStats::new(),
            BeliefStateTracker::default(),
            super::super::judge::JudgeConfig::default(),
            JudgeRecorder::default(),
        )
    }

    #[test]
    fn snapshot_reports_every_component_as_disabled_by_default() {
        let s = fresh_status().snapshot();
        assert_eq!(s["routing"]["enabled"], false);
        assert_eq!(s["self_consistency"]["enabled"], false);
        assert_eq!(s["belief_state"]["enabled"], false);
        assert_eq!(s["judge"]["enabled"], false);
    }

    #[test]
    fn snapshot_includes_every_top_level_key() {
        let s = fresh_status().snapshot();
        for k in ["routing", "self_consistency", "belief_state", "judge"] {
            assert!(s.get(k).is_some(), "missing key {k}: {s}");
        }
    }

    #[test]
    fn snapshot_includes_tracked_session_count() {
        let s = fresh_status();
        let v = s.snapshot();
        assert_eq!(v["belief_state"]["tracked_sessions"], 0);
    }
}
