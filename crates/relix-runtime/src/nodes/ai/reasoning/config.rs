//! `[reasoning.*]` config blocks.
//!
//! Operators turn on §7.29 components by adding the matching
//! TOML section to the AI controller's config. When a section
//! is absent the component stays off and the AI handler runs
//! pre-7.29 behaviour byte-for-byte.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Top-level `[reasoning]` section.
///
/// ```toml
/// [reasoning]
///
/// [reasoning.router]
/// enabled = true
/// fallback_to_default = true
///
/// [reasoning.router.tiers]
/// simple  = "google/gemini-2.0-flash"
/// medium  = "anthropic/claude-sonnet-4"
/// complex = "anthropic/claude-opus-4"
///
/// [reasoning.judge]
/// enabled    = true
/// model      = ""
/// threshold  = 2
/// apply_to   = ["tier3", "irreversible"]
///
/// [reasoning.belief]
/// enabled = true
/// db_path = "~/.relix/belief.db"
///
/// [reasoning.self_consistency]
/// enabled       = false
/// sample_count  = 3
/// apply_to      = ["tier3"]
/// ```
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ReasoningConfig {
    /// `[reasoning.router]` — Component 1 smart routing.
    #[serde(default)]
    pub router: Option<ReasoningRouterConfig>,
    /// `[reasoning.judge]` — Component 4 judge model.
    #[serde(default)]
    pub judge: Option<JudgeConfig>,
    /// `[reasoning.belief]` — Component 3 belief state.
    #[serde(default)]
    pub belief: Option<BeliefConfig>,
    /// `[reasoning.self_consistency]` — Component 2 signal.
    #[serde(default)]
    pub self_consistency: Option<SelfConsistencyConfig>,
}

/// `[reasoning.router]` section.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ReasoningRouterConfig {
    /// When `false` (default), the classifier is built but the
    /// dispatcher does not consult it — calls go to the
    /// provider's default model. Useful to dry-run the
    /// classifier in logs before flipping it on.
    #[serde(default)]
    pub enabled: bool,
    /// When `true` (default), a tier the operator did not
    /// configure falls back to the provider's default model.
    /// When `false`, an unconfigured tier returns an error so
    /// operators notice the gap before traffic flows.
    #[serde(default = "default_true")]
    pub fallback_to_default: bool,
    /// Per-tier model id mapping.
    #[serde(default)]
    pub tiers: RouterTiers,
}

/// `[reasoning.router.tiers]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct RouterTiers {
    /// Model id for Simple-tier calls. Empty = "use default".
    #[serde(default)]
    pub simple: String,
    /// Model id for Medium-tier calls.
    #[serde(default)]
    pub medium: String,
    /// Model id for Complex-tier calls.
    #[serde(default)]
    pub complex: String,
}

/// `[reasoning.judge]` section. Component 4.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct JudgeConfig {
    /// When `false` (default), the judge never runs.
    #[serde(default)]
    pub enabled: bool,
    /// Model id the judge uses. Empty = "the same provider's
    /// simple-tier model". Operators can set this to a
    /// different provider's model id for cross-provider
    /// independence.
    #[serde(default)]
    pub model: String,
    /// Number of flags (out of 5) at or above which the judge
    /// stops the action. Default 2 (= 2-flag warning + 3-flag
    /// stop per spec; operators tune higher for less-critical
    /// agents).
    #[serde(default = "default_judge_threshold")]
    pub threshold: u32,
    /// Which call categories the judge runs against. Default
    /// `["tier3", "irreversible"]` — Complex-tier requests AND
    /// any call the dispatcher tags as irreversible.
    #[serde(default = "default_apply_to")]
    pub apply_to: Vec<String>,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: String::new(),
            threshold: default_judge_threshold(),
            apply_to: default_apply_to(),
        }
    }
}

/// `[reasoning.belief]` section. Component 3.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct BeliefConfig {
    /// When `false` (default), the belief store is never
    /// opened. Pre-7.29 behaviour.
    #[serde(default)]
    pub enabled: bool,
    /// Path to the SQLite belief.db. Required when `enabled
    /// = true`; ignored otherwise.
    #[serde(default)]
    pub db_path: Option<PathBuf>,
    /// Confidence floor below which a belief is flagged as
    /// "needs resolution" by [`crate::nodes::ai::reasoning::belief::BeliefStore::list_needs_resolution`].
    /// Default 0.5.
    #[serde(default = "default_needs_resolution_floor")]
    pub needs_resolution_floor: f32,
}

/// `[reasoning.self_consistency]` section. Component 2 signal.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SelfConsistencyConfig {
    /// When `false` (default), self-consistency never runs.
    #[serde(default)]
    pub enabled: bool,
    /// Number of independent samples drawn for the
    /// self-consistency check. Default 3 per the spec.
    /// Operators tune up to 5 for high-stakes tiers; the cost
    /// is `sample_count×` the regular per-call price.
    #[serde(default = "default_sample_count")]
    pub sample_count: u32,
    /// Tier names this signal runs against. Default
    /// `["tier3"]` so it only fires on Complex-tier calls.
    #[serde(default = "default_self_consistency_apply_to")]
    pub apply_to: Vec<String>,
}

impl Default for SelfConsistencyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_count: default_sample_count(),
            apply_to: default_self_consistency_apply_to(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_judge_threshold() -> u32 {
    2
}

fn default_apply_to() -> Vec<String> {
    vec!["tier3".into(), "irreversible".into()]
}

fn default_needs_resolution_floor() -> f32 {
    0.5
}

fn default_sample_count() -> u32 {
    3
}

fn default_self_consistency_apply_to() -> Vec<String> {
    vec!["tier3".into()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_config_round_trips_from_toml() {
        let toml = r#"
            [router]
            enabled = true
            fallback_to_default = false

            [router.tiers]
            simple  = "openai/gpt-4o-mini"
            medium  = "openai/gpt-4o"
            complex = "openai/o1"
        "#;
        let cfg: ReasoningConfig = toml::from_str(toml).unwrap();
        let router = cfg.router.expect("router section parses");
        assert!(router.enabled);
        assert!(!router.fallback_to_default);
        assert_eq!(router.tiers.simple, "openai/gpt-4o-mini");
        assert_eq!(router.tiers.complex, "openai/o1");
    }

    #[test]
    fn empty_top_level_section_leaves_every_component_off() {
        let cfg: ReasoningConfig = toml::from_str("").unwrap();
        assert!(cfg.router.is_none());
        assert!(cfg.judge.is_none());
        assert!(cfg.belief.is_none());
        assert!(cfg.self_consistency.is_none());
    }

    #[test]
    fn judge_config_defaults_match_the_spec() {
        let toml = r#"
            [judge]
            enabled = true
        "#;
        let cfg: ReasoningConfig = toml::from_str(toml).unwrap();
        let j = cfg.judge.expect("judge section parses");
        assert!(j.enabled);
        assert_eq!(j.model, "");
        assert_eq!(j.threshold, 2);
        assert_eq!(j.apply_to, vec!["tier3".to_string(), "irreversible".into()]);
    }

    #[test]
    fn self_consistency_defaults_to_3_samples_tier3_only() {
        let toml = r#"
            [self_consistency]
            enabled = true
        "#;
        let cfg: ReasoningConfig = toml::from_str(toml).unwrap();
        let sc = cfg.self_consistency.expect("section parses");
        assert!(sc.enabled);
        assert_eq!(sc.sample_count, 3);
        assert_eq!(sc.apply_to, vec!["tier3".to_string()]);
    }

    #[test]
    fn belief_config_default_floor_is_half() {
        let toml = r#"
            [belief]
            enabled = true
            db_path = "/tmp/b.db"
        "#;
        let cfg: ReasoningConfig = toml::from_str(toml).unwrap();
        let b = cfg.belief.expect("belief section parses");
        assert!(b.enabled);
        assert!((b.needs_resolution_floor - 0.5).abs() < 1e-6);
    }
}
