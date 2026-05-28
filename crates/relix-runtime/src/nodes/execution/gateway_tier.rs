//! GAP 11 — three-tier transactional gateway types.
//!
//! Distinct from [`super::gateway`], which holds the legacy
//! in-memory `ActionGateway` (a per-call action log that
//! callers feed). This module adds the persistent, tier-aware
//! shape the §7.26 Component 5 spec describes:
//!
//! - **[`GatewayTier`]** — Tier A (auto-compensated), Tier B
//!   (human rollback plan), Tier C (blocked).
//! - **[`GatewayDispatchOptions`]** — per-call request shape
//!   carrying `transaction_id`, `idempotency_key`, `tier`, and
//!   `dry_run`. Callers (planner / direct invokers) pass this
//!   to the [`super::dispatcher::ToolDispatcher::dispatch_with_options`]
//!   surface; legacy callers using the bare `dispatch` keep
//!   their behaviour unchanged.
//! - **[`DryRunPreview`]** — what a dry-run dispatch returns
//!   instead of invoking the handler.
//! - **[`RollbackResult`]** — what the `execution.rollback`
//!   capability emits per transaction.
//!
//! The legacy `reversible: bool` parameter on
//! [`super::dispatcher::ToolDispatcher::dispatch`] continues
//! to work; when no tier is declared, the dispatcher classifies
//! the action as Tier B (the safe default — operator review
//! required if anything went wrong).

use serde::{Deserialize, Serialize};

/// One of the three transactional tiers.
///
/// The alpha stores tier as a JSON column on
/// `gateway_actions.tier`. Operators can read the persisted
/// shape directly without going through the runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tier", rename_all = "snake_case")]
pub enum GatewayTier {
    /// **Tier A — Auto-compensated.** The action declares an
    /// exact compensating call. On rollback the gateway
    /// executes the compensating tool with the declared
    /// arguments automatically. No human involvement.
    AutoCompensated {
        compensating_tool: String,
        compensating_args: serde_json::Value,
    },
    /// **Tier B — Human-rollback-plan.** The action is
    /// reversible but a human has to execute the rollback. The
    /// gateway records the plan text before committing so the
    /// operator can act on it.
    HumanRollbackPlan { rollback_plan: String },
    /// **Tier C — Blocked.** The action is never permitted
    /// regardless of who asks. The gateway rejects these
    /// before dispatch with [`super::dispatcher::DispatchError::AccessDenied`].
    Blocked { reason: String },
}

impl GatewayTier {
    /// Short tag used for the `tier` column when we want a
    /// fast string filter without parsing the JSON column.
    pub fn tag(&self) -> &'static str {
        match self {
            GatewayTier::AutoCompensated { .. } => "auto_compensated",
            GatewayTier::HumanRollbackPlan { .. } => "human_rollback",
            GatewayTier::Blocked { .. } => "blocked",
        }
    }
}

/// Per-call dispatch options. Used by the rich
/// `dispatch_with_options` entry point.
///
/// Backwards-compat: legacy callers pass [`Self::legacy`] which
/// maps the existing `reversible: bool` to a Tier B with the
/// supplied rollback hint as the plan.
#[derive(Clone, Debug, Default)]
pub struct GatewayDispatchOptions {
    /// Stable transaction id grouping every action in one
    /// `ai.chat` plan or operator-driven sequence. When
    /// `None`, the dispatcher generates one. Callers wanting
    /// to roll back a multi-step transaction must pass the
    /// same id on every dispatch.
    pub transaction_id: Option<String>,
    /// When set, the gateway treats two calls with the same
    /// (tool, idempotency_key) as one — the second call is
    /// skipped and returns the cached result. Prevents
    /// duplicate side effects on retry.
    pub idempotency_key: Option<String>,
    /// Tier the action should be classified as. When `None`,
    /// the dispatcher falls back to the legacy
    /// reversibility-based shape.
    pub tier: Option<GatewayTier>,
    /// When `true`, the handler is NOT invoked. The dispatcher
    /// returns a serialised [`DryRunPreview`] as the action's
    /// result and records the action with `dry_run = true`.
    pub dry_run: bool,
    /// Optional caller name. When set, recorded on the action
    /// row so `execution.rollback` can attribute the auto-
    /// compensating call back to the same actor.
    pub actor: Option<String>,
}

impl GatewayDispatchOptions {
    /// Legacy shape used by the existing
    /// `ToolDispatcher::dispatch(reversible, rollback_hint, ...)`.
    /// Always produces a Tier B classification so behaviour
    /// matches the pre-GAP-11 path.
    pub fn legacy(reversible: bool, rollback_hint: Option<String>) -> Self {
        let plan = match (reversible, rollback_hint.as_ref()) {
            (true, Some(h)) => h.clone(),
            (true, None) => String::new(),
            (false, Some(h)) => h.clone(),
            // Irreversible-with-no-hint stays a Tier B with an
            // empty plan; the dashboard surfaces the missing
            // plan as a warning row.
            (false, None) => String::new(),
        };
        Self {
            transaction_id: None,
            idempotency_key: None,
            tier: Some(GatewayTier::HumanRollbackPlan {
                rollback_plan: plan,
            }),
            dry_run: false,
            actor: None,
        }
    }

    /// Builder: pin the transaction id (caller-controlled).
    pub fn with_transaction_id(mut self, tx: impl Into<String>) -> Self {
        self.transaction_id = Some(tx.into());
        self
    }

    /// Builder: set the idempotency key.
    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    /// Builder: declare the action as Tier A.
    pub fn auto_compensated(
        mut self,
        compensating_tool: impl Into<String>,
        compensating_args: serde_json::Value,
    ) -> Self {
        self.tier = Some(GatewayTier::AutoCompensated {
            compensating_tool: compensating_tool.into(),
            compensating_args,
        });
        self
    }

    /// Builder: declare the action as Tier B.
    pub fn human_rollback_plan(mut self, plan: impl Into<String>) -> Self {
        self.tier = Some(GatewayTier::HumanRollbackPlan {
            rollback_plan: plan.into(),
        });
        self
    }

    /// Builder: declare the action as Tier C (blocked).
    pub fn blocked(mut self, reason: impl Into<String>) -> Self {
        self.tier = Some(GatewayTier::Blocked {
            reason: reason.into(),
        });
        self
    }

    /// Builder: flip dry-run mode on.
    pub fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }

    /// Builder: set the actor name (for cross-tier audit).
    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }
}

/// What dry-run mode returns as the action's "result" string.
/// Caller can JSON-decode and render — the format is the spec's
/// preview shape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryRunPreview {
    pub tool: String,
    pub args: String,
    pub tier_tag: String,
    pub would_be_reversible: bool,
    pub rollback_plan: Option<String>,
    pub compensating_tool: Option<String>,
}

impl DryRunPreview {
    /// Build a preview from the per-call options + the tool /
    /// args that would have been dispatched.
    pub fn build(tool: &str, args: &str, tier: &GatewayTier) -> Self {
        let (would_be_reversible, rollback_plan, compensating_tool) = match tier {
            GatewayTier::AutoCompensated {
                compensating_tool, ..
            } => (true, None, Some(compensating_tool.clone())),
            GatewayTier::HumanRollbackPlan { rollback_plan } => {
                (!rollback_plan.is_empty(), Some(rollback_plan.clone()), None)
            }
            GatewayTier::Blocked { reason } => (false, Some(reason.clone()), None),
        };
        DryRunPreview {
            tool: tool.to_string(),
            args: args.to_string(),
            tier_tag: tier.tag().to_string(),
            would_be_reversible,
            rollback_plan,
            compensating_tool,
        }
    }

    /// JSON serialisation used as the dispatch "result" string
    /// so existing consumers see a parseable payload.
    pub fn to_json_string(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// What `execution.rollback` returns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackResult {
    /// Tool / action_id pairs that were auto-rolled-back via
    /// their declared compensating call.
    pub auto_rolled_back: Vec<RollbackAction>,
    /// Tier B actions — the operator has to execute these by
    /// hand. The plan text is the rollback recipe.
    pub human_review_required: Vec<RollbackPlanItem>,
    /// Tier C actions that somehow ended up persisted. Should
    /// not happen (Tier C is rejected before dispatch); when
    /// it does the operator gets a loud error.
    pub errors: Vec<String>,
    /// The transaction id that was rolled back.
    pub transaction_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackAction {
    pub action_id: String,
    pub original_tool: String,
    pub compensating_tool: String,
    pub success: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackPlanItem {
    pub action_id: String,
    pub tool: String,
    pub rollback_plan: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_shape_maps_reversible_to_tier_b() {
        let opts = GatewayDispatchOptions::legacy(true, Some("re-fetch the data".into()));
        match opts.tier.unwrap() {
            GatewayTier::HumanRollbackPlan { rollback_plan } => {
                assert_eq!(rollback_plan, "re-fetch the data");
            }
            other => panic!("expected Tier B, got {other:?}"),
        }
    }

    #[test]
    fn legacy_shape_maps_irreversible_with_no_hint_to_empty_plan() {
        let opts = GatewayDispatchOptions::legacy(false, None);
        match opts.tier.unwrap() {
            GatewayTier::HumanRollbackPlan { rollback_plan } => {
                assert!(rollback_plan.is_empty());
            }
            other => panic!("expected Tier B, got {other:?}"),
        }
    }

    #[test]
    fn tier_tag_round_trip() {
        let a = GatewayTier::AutoCompensated {
            compensating_tool: "memory.delete".into(),
            compensating_args: json!({"id": 1}),
        };
        assert_eq!(a.tag(), "auto_compensated");
        let b = GatewayTier::HumanRollbackPlan {
            rollback_plan: "do thing".into(),
        };
        assert_eq!(b.tag(), "human_rollback");
        let c = GatewayTier::Blocked {
            reason: "never".into(),
        };
        assert_eq!(c.tag(), "blocked");
    }

    #[test]
    fn dry_run_preview_serialises_with_known_fields() {
        let tier = GatewayTier::AutoCompensated {
            compensating_tool: "memory.delete".into(),
            compensating_args: json!({"id": 1}),
        };
        let p = DryRunPreview::build("memory.write", r#"{"text":"hi"}"#, &tier);
        let s = p.to_json_string();
        assert!(s.contains("\"tier_tag\":\"auto_compensated\""));
        assert!(s.contains("\"would_be_reversible\":true"));
        assert!(s.contains("\"compensating_tool\":\"memory.delete\""));
    }

    #[test]
    fn builders_compose_into_expected_options() {
        let opts = GatewayDispatchOptions::default()
            .with_transaction_id("tx-1")
            .with_idempotency_key("k-1")
            .auto_compensated("memory.delete", json!({"id": 1}))
            .with_actor("alice");
        assert_eq!(opts.transaction_id.as_deref(), Some("tx-1"));
        assert_eq!(opts.idempotency_key.as_deref(), Some("k-1"));
        assert_eq!(opts.actor.as_deref(), Some("alice"));
        match opts.tier.unwrap() {
            GatewayTier::AutoCompensated {
                compensating_tool, ..
            } => {
                assert_eq!(compensating_tool, "memory.delete");
            }
            other => panic!("expected Tier A, got {other:?}"),
        }
    }

    #[test]
    fn blocked_builder_pins_reason() {
        let opts = GatewayDispatchOptions::default().blocked("rm -rf is never allowed");
        match opts.tier.unwrap() {
            GatewayTier::Blocked { reason } => {
                assert_eq!(reason, "rm -rf is never allowed");
            }
            other => panic!("expected Tier C, got {other:?}"),
        }
    }
}
