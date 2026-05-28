//! RELIX-7.19 — `FallbackEngine`.
//!
//! Given a [`super::ConfidenceScore`] and the capability the
//! call targeted, decides what action to take (pass / retry /
//! escalate / safe_default / alert / abort). The engine is a
//! pure-function dispatcher — it does NOT execute the
//! action; the dispatch bridge owns execution because only the
//! bridge has the handler registry + the in-flight context.
//!
//! Glob matching: capability patterns may carry `*` as a
//! suffix (`tool.*`), prefix (`*.chat`), or both
//! (`*backup*`). Literal patterns (no `*`) require an exact
//! match. The FIRST policy in the configured list whose
//! pattern matches wins — operators put narrower patterns
//! before broader ones.

use serde::{Deserialize, Serialize};

use super::config::{ConfidencePolicy, FallbackActionConfig};

/// Action the engine returns. Owned variants — operators
/// configure these once and the engine clones per-decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FallbackAction {
    /// Pass — return the response as-is.
    Pass,
    /// Re-dispatch the same cap up to `max_retries` times.
    Retry {
        max_retries: u32,
        retry_delay_ms: u64,
    },
    /// Re-dispatch to `escalate_to` with the same args.
    Escalate { escalate_to: String },
    /// Replace the response body with `default_value`.
    SafeDefault { default_value: String },
    /// Fire an alert; continue with the original response.
    Alert { alert_message: String },
    /// Return an error to the caller.
    Abort { abort_message: String },
}

impl FallbackAction {
    /// Whether this action mutates the response body. Used by
    /// the bridge to decide if the original outcome should be
    /// chronicled before swap.
    pub fn replaces_body(&self) -> bool {
        matches!(self, Self::SafeDefault { .. } | Self::Abort { .. })
    }
}

impl From<FallbackActionConfig> for FallbackAction {
    fn from(c: FallbackActionConfig) -> Self {
        match c {
            FallbackActionConfig::Pass => Self::Pass,
            FallbackActionConfig::Retry {
                max_retries,
                retry_delay_ms,
            } => Self::Retry {
                max_retries,
                retry_delay_ms,
            },
            FallbackActionConfig::Escalate { escalate_to } => Self::Escalate { escalate_to },
            FallbackActionConfig::SafeDefault { default_value } => {
                Self::SafeDefault { default_value }
            }
            FallbackActionConfig::Alert { alert_message } => Self::Alert { alert_message },
            FallbackActionConfig::Abort { abort_message } => Self::Abort { abort_message },
        }
    }
}

/// Verdict returned by [`FallbackEngine::decide`]. Carries
/// the matched policy index (for diagnostics +
/// chronicle) and the action to execute.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionVerdict {
    pub action: FallbackAction,
    /// True iff a policy MATCHED. `false` means the engine
    /// fell back to the default `Pass` action because no
    /// configured policy matched the capability.
    pub matched: bool,
    /// True iff the matched policy's `critical_threshold` was
    /// crossed (vs just `low_threshold`).
    pub critical: bool,
    /// RELIX-7.19 GAP 2: the matched policy's `low_threshold`.
    /// `None` when no policy matched. Used by the dispatch
    /// bridge to feed `AlertEngine::evaluate_low_confidence`
    /// without re-walking the policy list.
    pub low_threshold: Option<f32>,
    /// RELIX-7.19 GAP 2: the matched policy's
    /// `critical_threshold`. `None` when no policy matched.
    pub critical_threshold: Option<f32>,
}

impl ActionVerdict {
    pub fn pass() -> Self {
        Self {
            action: FallbackAction::Pass,
            matched: false,
            critical: false,
            low_threshold: None,
            critical_threshold: None,
        }
    }
}

/// The engine — owns a vector of resolved policies. Cheap to
/// clone (shared `Arc<Vec<…>>`).
#[derive(Clone, Default)]
pub struct FallbackEngine {
    policies: std::sync::Arc<Vec<ResolvedPolicy>>,
}

#[derive(Clone, Debug)]
struct ResolvedPolicy {
    pattern: String,
    low_threshold: f32,
    critical_threshold: f32,
    low_action: FallbackAction,
    critical_action: FallbackAction,
}

impl FallbackEngine {
    pub fn from_policies(policies: &[ConfidencePolicy]) -> Self {
        let resolved: Vec<ResolvedPolicy> = policies
            .iter()
            .map(|p| ResolvedPolicy {
                pattern: p.capability.clone(),
                low_threshold: p.low_threshold,
                critical_threshold: p.critical_threshold,
                low_action: p
                    .low_action
                    .clone()
                    .map(Into::into)
                    .unwrap_or(FallbackAction::Pass),
                critical_action: p
                    .critical_action
                    .clone()
                    .map(Into::into)
                    .unwrap_or(FallbackAction::Pass),
            })
            .collect();
        Self {
            policies: std::sync::Arc::new(resolved),
        }
    }

    /// Make a decision for one (capability, score) pair.
    /// O(P) over the policy list — typically small (single
    /// digits).
    pub fn decide(&self, capability: &str, score: f32) -> ActionVerdict {
        for p in self.policies.iter() {
            if !glob_match(&p.pattern, capability) {
                continue;
            }
            if score <= p.critical_threshold {
                return ActionVerdict {
                    action: p.critical_action.clone(),
                    matched: true,
                    critical: true,
                    low_threshold: Some(p.low_threshold),
                    critical_threshold: Some(p.critical_threshold),
                };
            }
            if score <= p.low_threshold {
                return ActionVerdict {
                    action: p.low_action.clone(),
                    matched: true,
                    critical: false,
                    low_threshold: Some(p.low_threshold),
                    critical_threshold: Some(p.critical_threshold),
                };
            }
            // Above both thresholds → pass; but we already
            // matched, so stop searching.
            return ActionVerdict {
                action: FallbackAction::Pass,
                matched: true,
                critical: false,
                low_threshold: Some(p.low_threshold),
                critical_threshold: Some(p.critical_threshold),
            };
        }
        ActionVerdict::pass()
    }

    /// Inspect the configured policies. Used by
    /// `confidence.policy_list`.
    pub fn list(&self) -> Vec<ListedPolicy> {
        self.policies
            .iter()
            .map(|p| ListedPolicy {
                capability: p.pattern.clone(),
                low_threshold: p.low_threshold,
                critical_threshold: p.critical_threshold,
                low_action: p.low_action.clone(),
                critical_action: p.critical_action.clone(),
            })
            .collect()
    }
}

/// Wire shape returned by `confidence.policy_list`. Mirrors
/// [`ConfidencePolicy`] but with the actions inlined post-
/// resolution.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListedPolicy {
    pub capability: String,
    pub low_threshold: f32,
    pub critical_threshold: f32,
    pub low_action: FallbackAction,
    pub critical_action: FallbackAction,
}

/// Simple glob matcher supporting `*` as suffix / prefix /
/// surround. Literal patterns require exact match. Used by
/// the engine; exposed `pub(crate)` so tests in adjacent
/// modules can reuse it.
pub(crate) fn glob_match(pattern: &str, value: &str) -> bool {
    let starts = pattern.starts_with('*');
    let ends = pattern.ends_with('*');
    let inner = pattern.trim_matches('*');
    match (starts, ends) {
        (false, false) => pattern == value,
        (true, false) => value.ends_with(inner),
        (false, true) => value.starts_with(inner),
        (true, true) => value.contains(inner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confidence::config::FallbackActionConfig;

    fn policy(
        capability: &str,
        low: f32,
        crit: f32,
        low_action: FallbackActionConfig,
        crit_action: FallbackActionConfig,
    ) -> ConfidencePolicy {
        ConfidencePolicy {
            capability: capability.into(),
            low_threshold: low,
            critical_threshold: crit,
            low_action: Some(low_action),
            critical_action: Some(crit_action),
        }
    }

    #[test]
    fn a_score_above_low_threshold_takes_the_pass_action() {
        let e = FallbackEngine::from_policies(&[policy(
            "ai.chat",
            0.5,
            0.3,
            FallbackActionConfig::Retry {
                max_retries: 2,
                retry_delay_ms: 500,
            },
            FallbackActionConfig::Abort {
                abort_message: "boom".into(),
            },
        )]);
        let v = e.decide("ai.chat", 0.9);
        assert!(v.matched);
        assert_eq!(v.action, FallbackAction::Pass);
    }

    #[test]
    fn a_score_below_low_threshold_takes_the_low_action() {
        let e = FallbackEngine::from_policies(&[policy(
            "ai.chat",
            0.5,
            0.3,
            FallbackActionConfig::Retry {
                max_retries: 2,
                retry_delay_ms: 500,
            },
            FallbackActionConfig::Abort {
                abort_message: "x".into(),
            },
        )]);
        let v = e.decide("ai.chat", 0.45);
        assert!(v.matched);
        assert!(!v.critical);
        assert!(matches!(
            v.action,
            FallbackAction::Retry { max_retries: 2, .. }
        ));
    }

    #[test]
    fn a_score_below_critical_threshold_takes_the_critical_action() {
        let e = FallbackEngine::from_policies(&[policy(
            "ai.chat",
            0.5,
            0.3,
            FallbackActionConfig::Retry {
                max_retries: 2,
                retry_delay_ms: 500,
            },
            FallbackActionConfig::Escalate {
                escalate_to: "ai.chat.premium".into(),
            },
        )]);
        let v = e.decide("ai.chat", 0.25);
        assert!(v.matched);
        assert!(v.critical);
        assert_eq!(
            v.action,
            FallbackAction::Escalate {
                escalate_to: "ai.chat.premium".into()
            }
        );
    }

    #[test]
    fn glob_matching_on_capability_tool_star() {
        let e = FallbackEngine::from_policies(&[policy(
            "tool.*",
            0.6,
            0.4,
            FallbackActionConfig::Alert {
                alert_message: "x".into(),
            },
            FallbackActionConfig::SafeDefault {
                default_value: "".into(),
            },
        )]);
        assert!(e.decide("tool.browser", 0.5).matched);
        assert!(e.decide("tool.code", 0.5).matched);
        assert!(!e.decide("ai.chat", 0.5).matched);
    }

    #[test]
    fn glob_prefix_wildcard_matches_suffix_string() {
        assert!(glob_match("*.chat", "ai.chat"));
        assert!(glob_match("*.chat", "premium.chat"));
        assert!(!glob_match("*.chat", "ai.embed"));
    }

    #[test]
    fn glob_surrounding_wildcard_matches_substring() {
        assert!(glob_match("*backup*", "tool.backup.run"));
        assert!(!glob_match("*backup*", "tool.restore"));
    }

    #[test]
    fn a_policy_with_no_match_falls_back_to_pass() {
        let e = FallbackEngine::from_policies(&[policy(
            "ai.chat",
            0.5,
            0.3,
            FallbackActionConfig::Alert {
                alert_message: "x".into(),
            },
            FallbackActionConfig::Abort {
                abort_message: "y".into(),
            },
        )]);
        let v = e.decide("tool.browser", 0.1);
        assert!(!v.matched);
        assert_eq!(v.action, FallbackAction::Pass);
    }

    #[test]
    fn first_matching_policy_wins() {
        let e = FallbackEngine::from_policies(&[
            policy(
                "ai.chat",
                0.5,
                0.3,
                FallbackActionConfig::Retry {
                    max_retries: 2,
                    retry_delay_ms: 500,
                },
                FallbackActionConfig::Abort {
                    abort_message: "x".into(),
                },
            ),
            policy(
                "ai.*",
                0.8,
                0.6,
                FallbackActionConfig::Alert {
                    alert_message: "y".into(),
                },
                FallbackActionConfig::Abort {
                    abort_message: "z".into(),
                },
            ),
        ]);
        // ai.chat at 0.45 - narrower policy says low_action (retry).
        let v = e.decide("ai.chat", 0.45);
        assert!(matches!(
            v.action,
            FallbackAction::Retry { max_retries: 2, .. }
        ));
        // ai.embed only matched by the broader policy.
        let v = e.decide("ai.embed", 0.75);
        assert!(matches!(v.action, FallbackAction::Alert { .. }));
    }

    #[test]
    fn safe_default_and_abort_replace_body_other_actions_do_not() {
        assert!(
            FallbackAction::SafeDefault {
                default_value: "".into()
            }
            .replaces_body()
        );
        assert!(
            FallbackAction::Abort {
                abort_message: "x".into()
            }
            .replaces_body()
        );
        assert!(!FallbackAction::Pass.replaces_body());
        assert!(
            !FallbackAction::Retry {
                max_retries: 1,
                retry_delay_ms: 100
            }
            .replaces_body()
        );
    }
}
