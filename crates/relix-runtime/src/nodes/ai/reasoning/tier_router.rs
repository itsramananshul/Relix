//! GAP 16 Component 1 — tier → model id resolver.
//!
//! Wraps a [`RouterTiers`] config block and answers
//! "what model id should this tier dispatch to". When the
//! configured tier model is empty, falls back to either the
//! provider's default (when `fallback_to_default = true`,
//! the default) OR an error (when the operator set it
//! `false` to surface the configuration gap).

use serde::{Deserialize, Serialize};

use super::classifier::ReasoningTier;
use super::config::{ReasoningRouterConfig, RouterTiers};

/// Errors raised by [`TierRouter::resolve`].
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq, Serialize, Deserialize)]
pub enum TierRouterError {
    /// The tier has no configured model id AND the operator
    /// turned `fallback_to_default = false` so the call has
    /// nowhere to land.
    #[error("tier router: no model configured for tier '{tier}' and fallback_to_default = false")]
    UnconfiguredTier {
        /// String form of the unmapped tier.
        tier: String,
    },
}

/// Resolve a tier into a model id.
#[derive(Clone, Debug, Default)]
pub struct TierRouter {
    cfg: ReasoningRouterConfig,
}

impl TierRouter {
    /// Construct from a parsed router config. Cheap clone.
    pub fn new(cfg: ReasoningRouterConfig) -> Self {
        Self { cfg }
    }

    /// Is the router operationally on? When `false`, the AI
    /// handler skips the tier-routing branch entirely.
    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Borrow the configured per-tier map.
    pub fn tiers(&self) -> &RouterTiers {
        &self.cfg.tiers
    }

    /// Resolve a tier to a model id.
    ///
    /// Returns `Ok(Some(model_id))` when the tier has a
    /// non-empty configured model. Returns `Ok(None)` when
    /// the tier is unmapped AND `fallback_to_default = true`
    /// (the AI handler interprets `None` as "use the
    /// provider's default model"). Returns
    /// `Err(UnconfiguredTier)` when the tier is unmapped AND
    /// fallback is disabled.
    pub fn resolve(&self, tier: ReasoningTier) -> Result<Option<String>, TierRouterError> {
        let model = match tier {
            ReasoningTier::Simple => self.cfg.tiers.simple.trim(),
            ReasoningTier::Medium => self.cfg.tiers.medium.trim(),
            ReasoningTier::Complex => self.cfg.tiers.complex.trim(),
        };
        if !model.is_empty() {
            return Ok(Some(model.to_string()));
        }
        if self.cfg.fallback_to_default {
            Ok(None)
        } else {
            Err(TierRouterError::UnconfiguredTier {
                tier: tier.as_str().to_string(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(simple: &str, medium: &str, complex: &str, fallback: bool) -> ReasoningRouterConfig {
        ReasoningRouterConfig {
            enabled: true,
            fallback_to_default: fallback,
            tiers: RouterTiers {
                simple: simple.into(),
                medium: medium.into(),
                complex: complex.into(),
            },
        }
    }

    #[test]
    fn resolve_returns_the_configured_model_per_tier() {
        let r = TierRouter::new(cfg("s", "m", "c", true));
        assert_eq!(
            r.resolve(ReasoningTier::Simple).unwrap().as_deref(),
            Some("s")
        );
        assert_eq!(
            r.resolve(ReasoningTier::Medium).unwrap().as_deref(),
            Some("m")
        );
        assert_eq!(
            r.resolve(ReasoningTier::Complex).unwrap().as_deref(),
            Some("c")
        );
    }

    #[test]
    fn resolve_returns_none_when_tier_unset_and_fallback_on() {
        let r = TierRouter::new(cfg("", "m", "c", true));
        assert_eq!(r.resolve(ReasoningTier::Simple).unwrap(), None);
    }

    #[test]
    fn resolve_errors_when_tier_unset_and_fallback_off() {
        let r = TierRouter::new(cfg("", "m", "c", false));
        let err = r.resolve(ReasoningTier::Simple).unwrap_err();
        match err {
            TierRouterError::UnconfiguredTier { tier } => assert_eq!(tier, "simple"),
        }
    }

    #[test]
    fn resolve_trims_whitespace_from_configured_ids() {
        let r = TierRouter::new(cfg("  gpt-4o-mini  ", "m", "c", true));
        assert_eq!(
            r.resolve(ReasoningTier::Simple).unwrap().as_deref(),
            Some("gpt-4o-mini")
        );
    }

    #[test]
    fn enabled_mirrors_the_config_flag() {
        let r = TierRouter::new(ReasoningRouterConfig {
            enabled: false,
            ..Default::default()
        });
        assert!(!r.enabled());
    }
}
