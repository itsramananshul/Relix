//! Policy engine — evaluates an [`super::ExecutionPlan`]
//! against operator-configured rules and returns a
//! [`PolicyVerdict`] the executor consults before running.
//!
//! The engine is pure logic + a small config struct; the
//! controller-runtime builds one from `[execution] …`
//! settings and passes it to the AI handler.

use serde::Deserialize;

use super::planner::{ExecutionPlan, Reversibility};

/// `[execution]` config block. Defaults match the documented
/// "default policy" — most deployments should not have to
/// touch these.
#[derive(Clone, Debug, Deserialize)]
pub struct ExecutionConfig {
    /// Whether plans containing `Irreversible` steps may run
    /// without operator approval. Default `false` so a
    /// freshly-installed controller refuses to send
    /// irreversible actions until the operator wires up an
    /// approval surface.
    #[serde(default)]
    pub allow_irreversible: bool,
    /// Plans whose estimated cost exceeds this floor must
    /// gather an approval token before running. Default
    /// 1000 (= $10).
    #[serde(default = "default_cost_floor")]
    pub require_approval_above_cost_cents: u32,
    /// Maximum plan length. Plans over this size are denied
    /// outright — runaway agent plans should not be allowed
    /// to spiral. Default 32.
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
    /// Whether the executor captures per-step evidence into
    /// the chronicle. Default `true`.
    #[serde(default = "default_evidence_capture")]
    pub evidence_capture: bool,
    /// Whether irreversible plans require an
    /// `approval_token` in the request context. Default
    /// `true` — pairs with `allow_irreversible = false`.
    #[serde(default = "default_require_approval_for_irreversible")]
    pub require_approval_for_irreversible: bool,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            allow_irreversible: false,
            require_approval_above_cost_cents: default_cost_floor(),
            max_steps: default_max_steps(),
            evidence_capture: default_evidence_capture(),
            require_approval_for_irreversible: default_require_approval_for_irreversible(),
        }
    }
}

fn default_cost_floor() -> u32 {
    1000
}

fn default_max_steps() -> usize {
    32
}

fn default_evidence_capture() -> bool {
    true
}

fn default_require_approval_for_irreversible() -> bool {
    true
}

/// Verdict returned by [`PolicyEngine::evaluate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicyVerdict {
    /// Plan may run as-is.
    Approved,
    /// Plan needs an operator approval token before it can
    /// run. `reason` is operator-readable.
    RequiresApproval { reason: String },
    /// Plan is denied; the runtime should not run it even
    /// with an approval token. `reason` is the operator-
    /// readable rejection message.
    Denied { reason: String },
}

/// Policy engine. Holds the per-controller config; the
/// `evaluate` method is pure logic.
#[derive(Clone, Debug)]
pub struct PolicyEngine {
    pub allow_irreversible: bool,
    pub require_approval_above_cost_cents: u32,
    pub max_steps: usize,
}

impl PolicyEngine {
    pub fn from_config(cfg: &ExecutionConfig) -> Self {
        Self {
            allow_irreversible: cfg.allow_irreversible,
            require_approval_above_cost_cents: cfg.require_approval_above_cost_cents,
            max_steps: cfg.max_steps,
        }
    }

    /// Sensible default for tests + first-boot controllers.
    pub fn default_policy() -> Self {
        Self::from_config(&ExecutionConfig::default())
    }

    /// Evaluate the plan. Order is deny-first, then
    /// require-approval, then approve: a plan that's both
    /// over-budget AND too long fails the deny check first
    /// so the operator sees the highest-severity reason.
    pub fn evaluate(&self, plan: &ExecutionPlan) -> PolicyVerdict {
        if plan.steps.len() > self.max_steps {
            return PolicyVerdict::Denied {
                reason: format!(
                    "plan has {} steps; max_steps = {}",
                    plan.steps.len(),
                    self.max_steps
                ),
            };
        }
        match plan.reversibility {
            Reversibility::Irreversible | Reversibility::PartiallyReversible { .. } => {
                if !self.allow_irreversible {
                    return PolicyVerdict::RequiresApproval {
                        reason: format!(
                            "plan reversibility = {}; allow_irreversible = false",
                            plan.reversibility.as_str()
                        ),
                    };
                }
            }
            Reversibility::Reversible => {}
        }
        if plan.estimated_cost_cents > self.require_approval_above_cost_cents {
            return PolicyVerdict::RequiresApproval {
                reason: format!(
                    "estimated cost {}c exceeds approval floor {}c",
                    plan.estimated_cost_cents, self.require_approval_above_cost_cents
                ),
            };
        }
        PolicyVerdict::Approved
    }
}

#[cfg(test)]
mod tests {
    use super::super::planner::{PlanStep, Reversibility};
    use super::*;

    fn small_reversible_plan() -> ExecutionPlan {
        ExecutionPlan {
            steps: vec![PlanStep::ModelCall {
                prompt: "hi".into(),
                model: "m".into(),
            }],
            estimated_cost_cents: 1,
            requires_approval: false,
            reversibility: Reversibility::Reversible,
        }
    }

    fn irreversible_plan() -> ExecutionPlan {
        ExecutionPlan {
            steps: vec![PlanStep::ToolCall {
                tool: "email.send".into(),
                args: "x".into(),
            }],
            estimated_cost_cents: 10,
            requires_approval: true,
            reversibility: Reversibility::Irreversible,
        }
    }

    #[test]
    fn default_policy_approves_small_reversible_plan() {
        let p = PolicyEngine::default_policy();
        assert_eq!(
            p.evaluate(&small_reversible_plan()),
            PolicyVerdict::Approved
        );
    }

    #[test]
    fn policy_denies_plan_exceeding_max_steps() {
        let mut p = PolicyEngine::default_policy();
        p.max_steps = 2;
        let plan = ExecutionPlan {
            steps: vec![
                PlanStep::ModelCall {
                    prompt: "a".into(),
                    model: "m".into(),
                },
                PlanStep::ModelCall {
                    prompt: "b".into(),
                    model: "m".into(),
                },
                PlanStep::ModelCall {
                    prompt: "c".into(),
                    model: "m".into(),
                },
            ],
            estimated_cost_cents: 0,
            requires_approval: false,
            reversibility: Reversibility::Reversible,
        };
        match p.evaluate(&plan) {
            PolicyVerdict::Denied { reason } => {
                assert!(reason.contains("max_steps = 2"));
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn policy_requires_approval_above_cost_threshold() {
        let mut p = PolicyEngine::default_policy();
        p.require_approval_above_cost_cents = 500;
        let plan = ExecutionPlan {
            steps: vec![PlanStep::ModelCall {
                prompt: "x".into(),
                model: "m".into(),
            }],
            estimated_cost_cents: 1500,
            requires_approval: false,
            reversibility: Reversibility::Reversible,
        };
        match p.evaluate(&plan) {
            PolicyVerdict::RequiresApproval { reason } => {
                assert!(reason.contains("1500c"));
                assert!(reason.contains("500c"));
            }
            other => panic!("expected RequiresApproval, got {other:?}"),
        }
    }

    #[test]
    fn policy_requires_approval_for_irreversible_when_not_allowed() {
        let p = PolicyEngine::default_policy();
        match p.evaluate(&irreversible_plan()) {
            PolicyVerdict::RequiresApproval { reason } => {
                assert!(reason.contains("irreversible"));
            }
            other => panic!("expected RequiresApproval, got {other:?}"),
        }
    }

    #[test]
    fn policy_allows_irreversible_when_flag_set() {
        let mut p = PolicyEngine::default_policy();
        p.allow_irreversible = true;
        // The irreversible plan has cost 10c, well below the
        // 1000c default floor, so it sails through.
        assert_eq!(p.evaluate(&irreversible_plan()), PolicyVerdict::Approved);
    }

    #[test]
    fn policy_deny_takes_priority_over_require_approval() {
        let mut p = PolicyEngine::default_policy();
        p.max_steps = 0; // any non-empty plan denied
        // Build a plan that would also trip the cost floor.
        let mut plan = small_reversible_plan();
        plan.estimated_cost_cents = 9_999;
        match p.evaluate(&plan) {
            PolicyVerdict::Denied { .. } => {}
            other => panic!("Denied should win over RequiresApproval, got {other:?}"),
        }
    }

    #[test]
    fn execution_config_defaults_match_documented_values() {
        let cfg = ExecutionConfig::default();
        assert!(!cfg.allow_irreversible);
        assert_eq!(cfg.require_approval_above_cost_cents, 1000);
        assert_eq!(cfg.max_steps, 32);
        assert!(cfg.evidence_capture);
        assert!(cfg.require_approval_for_irreversible);
    }
}
