//! RELIX-7.19 — `[confidence]` TOML schema.
//!
//! Operators wire confidence scoring + fallback under the
//! `[confidence]` block on any node that hosts a dispatch
//! bridge (typically every node). The block is fully optional;
//! absence leaves the scorer un-wired and the bridge's hot
//! path byte-for-byte identical to pre-7.19.
//!
//! ```toml
//! [confidence]
//! enabled = true
//! window_size = 100              # rolling window per (agent, method)
//! p95_latency_baseline_ms = 1500 # latency_signal anchor
//! error_rate_discount = 0.5      # multiplier applied when error_rate >= 0.5
//!
//! [confidence.weights]
//! response_length    = 0.20
//! response_coherence = 0.25
//! provider_signal    = 0.30
//! error_rate_history = 0.15
//! latency_signal     = 0.10
//!
//! [[confidence.policies]]
//! capability = "ai.chat"
//! low_threshold = 0.5
//! critical_threshold = 0.3
//!
//!   [confidence.policies.low_action]
//!   type = "retry"
//!   max_retries = 2
//!   retry_delay_ms = 500
//!
//!   [confidence.policies.critical_action]
//!   type = "escalate"
//!   escalate_to = "ai.chat.premium"
//!
//! [[confidence.policies]]
//! capability = "tool.*"
//! low_threshold = 0.6
//! critical_threshold = 0.4
//!
//!   [confidence.policies.low_action]
//!   type = "alert"
//!   alert_message = "Tool confidence low"
//!
//!   [confidence.policies.critical_action]
//!   type = "safe_default"
//!   default_value = ""
//! ```

use serde::{Deserialize, Serialize};

/// Top-level `[confidence]` block.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConfidenceConfig {
    /// Master switch. `false` (or block absent) leaves the
    /// dispatch bridge unmodified — pre-7.19 byte-for-byte.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Per-(agent, method) rolling-window depth. Defaults to
    /// 100 — matches the spec.
    #[serde(default = "default_window_size")]
    pub window_size: usize,
    /// Latency anchor used by the `latency_signal` sub-score.
    /// Responses faster than this score full marks; slower
    /// scale down linearly to 0.0 at 4× the baseline.
    #[serde(default = "default_p95_baseline")]
    pub p95_latency_baseline_ms: u64,
    /// Multiplier applied to the final score when the rolling
    /// error rate is at-or-above 0.5. 0.5 means "halve the
    /// score when half of recent calls failed." 1.0 disables
    /// the discount. Defaults to 0.5.
    #[serde(default = "default_error_rate_discount")]
    pub error_rate_discount: f32,
    /// Weights for each sub-score. Defaults sum to 1.0 — the
    /// final score is the simple weighted average.
    #[serde(default)]
    pub weights: ConfidenceWeights,
    /// Per-capability fallback policies. Capability strings
    /// support glob suffixes (`tool.*` matches anything
    /// starting with `tool.`). The FIRST matching policy
    /// wins; configure narrower patterns first.
    #[serde(default)]
    pub policies: Vec<ConfidencePolicy>,
    /// RELIX-7.29 PART 2 — `[confidence.self_consistency]`
    /// adaptive sampling block. When `enabled = true` AND the
    /// dispatched capability matches `capability_patterns`
    /// AND the baseline confidence drops below
    /// `min_score_to_enable`, the AI handler fires
    /// `sample_count` parallel `ai.chat` retries via
    /// `tokio::join_all`, embeds each sample's core answer,
    /// averages pairwise cosine similarity, and feeds the
    /// score into the scorer where it REPLACES the
    /// `provider_signal` sub-score. Absent / `enabled = false`
    /// keeps the AI handler byte-identical to its pre-SC
    /// behaviour.
    #[serde(default)]
    pub self_consistency: Option<super::self_consistency::SelfConsistencyConfig>,
}

impl Default for ConfidenceConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            window_size: default_window_size(),
            p95_latency_baseline_ms: default_p95_baseline(),
            error_rate_discount: default_error_rate_discount(),
            weights: ConfidenceWeights::default(),
            policies: Vec::new(),
            self_consistency: None,
        }
    }
}

fn default_enabled() -> bool {
    false
}
fn default_window_size() -> usize {
    100
}
fn default_p95_baseline() -> u64 {
    1500
}
fn default_error_rate_discount() -> f32 {
    0.5
}

/// Weighted contributions of each sub-score to the final
/// confidence value. Weights are normalised to sum=1.0 at
/// scoring time so operators can set absolute weights and the
/// engine handles the bookkeeping.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct ConfidenceWeights {
    #[serde(default = "weight_length")]
    pub response_length: f32,
    #[serde(default = "weight_coherence")]
    pub response_coherence: f32,
    #[serde(default = "weight_provider")]
    pub provider_signal: f32,
    #[serde(default = "weight_error_rate")]
    pub error_rate_history: f32,
    #[serde(default = "weight_latency")]
    pub latency_signal: f32,
}

impl Default for ConfidenceWeights {
    fn default() -> Self {
        Self {
            response_length: weight_length(),
            response_coherence: weight_coherence(),
            provider_signal: weight_provider(),
            error_rate_history: weight_error_rate(),
            latency_signal: weight_latency(),
        }
    }
}

fn weight_length() -> f32 {
    0.20
}
fn weight_coherence() -> f32 {
    0.25
}
fn weight_provider() -> f32 {
    0.30
}
fn weight_error_rate() -> f32 {
    0.15
}
fn weight_latency() -> f32 {
    0.10
}

impl ConfidenceWeights {
    /// Sum of all weights. Used to normalise the per-sub-score
    /// contributions. `0.0` is treated as `1.0` to avoid a
    /// divide-by-zero — the scorer falls back to an
    /// unweighted average in that degenerate case.
    pub fn total(&self) -> f32 {
        let t = self.response_length
            + self.response_coherence
            + self.provider_signal
            + self.error_rate_history
            + self.latency_signal;
        if t > f32::EPSILON { t } else { 1.0 }
    }
}

/// One configured fallback policy.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConfidencePolicy {
    /// Capability matcher. Either a literal cap name
    /// (`"ai.chat"`) or a glob with `*` as a suffix
    /// (`"tool.*"`), prefix (`"*.chat"`), or both
    /// (`"*backup*"`).
    pub capability: String,
    /// Final score at-or-below this triggers `low_action`.
    /// Defaults to 0.5.
    #[serde(default = "default_low_threshold")]
    pub low_threshold: f32,
    /// Final score at-or-below this triggers
    /// `critical_action`. Defaults to 0.3.
    #[serde(default = "default_critical_threshold")]
    pub critical_threshold: f32,
    /// What to do when `low_threshold` is crossed (but not
    /// critical). `None` = pass.
    #[serde(default)]
    pub low_action: Option<FallbackActionConfig>,
    /// What to do when `critical_threshold` is crossed.
    /// `None` = pass.
    #[serde(default)]
    pub critical_action: Option<FallbackActionConfig>,
}

fn default_low_threshold() -> f32 {
    0.5
}
fn default_critical_threshold() -> f32 {
    0.3
}

/// Wire-format fallback action. Deserialised via internally-
/// tagged `type = "..."` so operators write
/// `{ type = "retry", max_retries = 2, retry_delay_ms = 500 }`
/// in TOML.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FallbackActionConfig {
    /// No-op. Return the response as-is.
    Pass,
    /// Re-dispatch the same capability call up to `max_retries`
    /// times. Sleep `retry_delay_ms` between attempts. Stop
    /// when confidence rises above the threshold or retries
    /// exhaust.
    Retry {
        #[serde(default = "default_max_retries")]
        max_retries: u32,
        #[serde(default = "default_retry_delay_ms")]
        retry_delay_ms: u64,
    },
    /// Re-dispatch to a different capability. The escalated
    /// call carries the same args + caller identity but
    /// targets `escalate_to`. The escalated response gets its
    /// own confidence check (NOT recursive — the engine emits
    /// a `pass` for the escalated cap to avoid infinite loops).
    Escalate { escalate_to: String },
    /// Replace the low-confidence response body with
    /// `default_value`. The original response is logged for
    /// audit before being dropped.
    SafeDefault {
        #[serde(default)]
        default_value: String,
    },
    /// Send an alert via the operator-wired alert mesh. Continue
    /// with the original response after firing the alert.
    Alert {
        #[serde(default = "default_alert_message")]
        alert_message: String,
    },
    /// Return an `INVALID_ARGS`-flavoured error to the caller.
    /// Used when a wrong answer is worse than no answer.
    Abort {
        #[serde(default = "default_abort_message")]
        abort_message: String,
    },
}

fn default_max_retries() -> u32 {
    1
}
fn default_retry_delay_ms() -> u64 {
    200
}
fn default_alert_message() -> String {
    "confidence below threshold".into()
}
fn default_abort_message() -> String {
    "confidence below critical threshold; aborting".into()
}

/// Static descriptor pairs for the `confidence.*` capability
/// surface. Mirrors `sharing_group_descriptors()` etc. The
/// controller-runtime builds [`relix_core::capability::CapabilityDescriptor`]s
/// from this list.
pub fn confidence_capability_descriptors() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "confidence.policy_list",
            "RELIX-7.19: list every configured confidence \
             policy as JSON. No args. Each entry has \
             `capability`, `low_threshold`, `critical_threshold`, \
             `low_action`, `critical_action`. Operators use \
             this to confirm the wired-in policy matches their \
             intent.",
        ),
        (
            "confidence.score_history",
            "RELIX-7.19: rolling-window snapshot for one \
             (agent, method) pair. Args JSON: `{agent, method}`. \
             Returns `{call_count, error_count, error_rate, \
             p50_latency_ms, p95_latency_ms, p99_latency_ms, \
             avg_confidence}`. Useful for dashboards that need \
             per-agent confidence trends without scanning the \
             whole metrics store.",
        ),
        (
            "confidence.reset_history",
            "RELIX-7.19: clear the rolling-window state for a \
             specific (agent, method) pair OR every method on \
             one agent. Args JSON: `{agent, method?}` — when \
             `method` is omitted, every per-method window for \
             that agent is cleared. Used after a provider \
             incident recovers and operators want the error-rate \
             discount to reset immediately rather than waiting \
             for the window to roll over.",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_disabled_with_documented_defaults() {
        let c = ConfidenceConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.window_size, 100);
        assert_eq!(c.p95_latency_baseline_ms, 1500);
        assert!((c.error_rate_discount - 0.5).abs() < 1e-6);
    }

    #[test]
    fn default_weights_sum_to_one() {
        let w = ConfidenceWeights::default();
        let total = w.response_length
            + w.response_coherence
            + w.provider_signal
            + w.error_rate_history
            + w.latency_signal;
        assert!((total - 1.0).abs() < 1e-6, "got {total}");
    }

    #[test]
    fn parses_full_toml_block() {
        let cfg: ConfidenceConfig = toml::from_str(
            r#"
            enabled = true
            window_size = 50
            p95_latency_baseline_ms = 800
            error_rate_discount = 0.6

            [weights]
            response_length    = 0.10
            response_coherence = 0.20
            provider_signal    = 0.40
            error_rate_history = 0.20
            latency_signal     = 0.10

            [[policies]]
            capability = "ai.chat"
            low_threshold = 0.6
            critical_threshold = 0.4

            [policies.low_action]
            type = "retry"
            max_retries = 3
            retry_delay_ms = 500

            [policies.critical_action]
            type = "escalate"
            escalate_to = "ai.chat.premium"

            [[policies]]
            capability = "tool.*"

            [policies.low_action]
            type = "alert"
            alert_message = "tool wobble"

            [policies.critical_action]
            type = "safe_default"
            default_value = ""
            "#,
        )
        .expect("parse");
        assert!(cfg.enabled);
        assert_eq!(cfg.window_size, 50);
        assert_eq!(cfg.policies.len(), 2);
        assert_eq!(cfg.policies[0].capability, "ai.chat");
        assert!(matches!(
            cfg.policies[0].low_action,
            Some(FallbackActionConfig::Retry { max_retries: 3, .. })
        ));
        assert!(matches!(
            cfg.policies[1].critical_action,
            Some(FallbackActionConfig::SafeDefault { .. })
        ));
    }

    #[test]
    fn weights_total_avoids_divide_by_zero() {
        let w = ConfidenceWeights {
            response_length: 0.0,
            response_coherence: 0.0,
            provider_signal: 0.0,
            error_rate_history: 0.0,
            latency_signal: 0.0,
        };
        assert!((w.total() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn descriptors_cover_every_capability() {
        let methods: Vec<&str> = confidence_capability_descriptors()
            .iter()
            .map(|(m, _)| *m)
            .collect();
        for expected in [
            "confidence.policy_list",
            "confidence.score_history",
            "confidence.reset_history",
        ] {
            assert!(methods.contains(&expected), "missing: {expected}");
        }
    }
}
