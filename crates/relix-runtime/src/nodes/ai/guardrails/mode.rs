//! Guardrail mode — operator-facing calibration knob that
//! picks the right blend of strictness for a deployment.
//!
//! The mode controls three orthogonal sub-policies:
//!
//! - whether the **injection check** is on (almost always
//!   yes, but operators running internal-only deployments
//!   may opt out),
//! - the **PII policy** (allow / redact / block),
//! - and the **category gate** — whether to admit prompts
//!   tagged `medical_query` / `security_query` / `legal_query`
//!   / `creative_writing` / `code_request`.
//!
//! ## Hard stops
//!
//! Regardless of mode, the system NEVER serves prompts that:
//!
//! 1. Try to extract credentials from Relix internals
//!    (caught by the policy admit layer before this code
//!    even runs).
//! 2. Match [`crate::nodes::memory::guard::MemoryGuard`]
//!    poison patterns (enforced at the memory write
//!    boundary, separate concern).
//! 3. Would push the controller past its configured cost cap
//!    (enforced by the budget guard, separate concern).
//!
//! Those guards run BEFORE the mode is consulted; the mode
//! only governs the additive request-time checks.

use super::input::PiiPolicy;
use serde::Deserialize;

/// Three-level operator calibration. `Balanced` is the
/// default and what most deployments should run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailMode {
    /// Refuse anything suspicious. Sensitive categories are
    /// blocked; PII is blocked outright.
    Strict,
    /// Use context to distinguish. Sensitive categories are
    /// allowed; PII is redacted.
    #[default]
    Balanced,
    /// Only block obvious violations. Everything category-
    /// gated passes; PII flows through. Injection check
    /// stays on regardless — it's a hard requirement.
    Permissive,
}

impl GuardrailMode {
    /// Parse from a config / CLI string. Returns `None` for
    /// unknown values so the caller can log a clear
    /// "unknown mode `X`" error rather than silently picking
    /// a default.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "strict" => Some(Self::Strict),
            "balanced" => Some(Self::Balanced),
            "permissive" => Some(Self::Permissive),
            _ => None,
        }
    }

    /// Stable lower-case tag. Matches the serde wire form
    /// (`#[serde(rename_all = "snake_case")]`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Balanced => "balanced",
            Self::Permissive => "permissive",
        }
    }

    /// Whether prompts tagged with the named category should
    /// be allowed at this mode level. Categories are the
    /// stable tags from
    /// [`crate::nodes::ai::guardrails::categories`].
    pub fn allows_category(&self, category: &str) -> bool {
        let cat = strip_prefix(category);
        match self {
            // Strict refuses sensitive categories. Creative
            // + code stay off too: a strict deployment is
            // typically a customer-support / compliance bot
            // that has no business writing fiction or new
            // code.
            Self::Strict => !matches!(cat, "medical" | "security" | "legal" | "creative" | "code"),
            Self::Balanced => true,
            Self::Permissive => true,
        }
    }

    /// PII policy implied by this mode.
    pub fn pii_policy(&self) -> PiiPolicy {
        match self {
            Self::Strict => PiiPolicy::Block,
            Self::Balanced => PiiPolicy::Redact,
            Self::Permissive => PiiPolicy::Allow,
        }
    }

    /// Whether the injection-check runs. Strict / Balanced /
    /// Permissive all want this on — injection attempts are
    /// the one universal failure mode. Operators who need
    /// the check off can still build a custom
    /// [`super::InputGuardrail`] directly.
    pub fn injection_check(&self) -> bool {
        true
    }
}

/// `category` values from [`super::categories`] use suffixes
/// like `medical_query` / `creative_writing`. The mode
/// vocabulary is shorter (`medical` / `creative`); strip the
/// suffix so both forms work.
fn strip_prefix(category: &str) -> &str {
    if let Some(rest) = category.strip_suffix("_query") {
        return rest;
    }
    if let Some(rest) = category.strip_suffix("_writing") {
        return rest;
    }
    if let Some(rest) = category.strip_suffix("_request") {
        return rest;
    }
    category
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::ai::guardrails::categories;

    #[test]
    fn parse_round_trips_via_string() {
        for m in [
            GuardrailMode::Strict,
            GuardrailMode::Balanced,
            GuardrailMode::Permissive,
        ] {
            assert_eq!(GuardrailMode::parse(m.as_str()), Some(m));
        }
        // Case-insensitive parse.
        assert_eq!(
            GuardrailMode::parse("BALANCED"),
            Some(GuardrailMode::Balanced)
        );
        // Unknown returns None.
        assert!(GuardrailMode::parse("garbage").is_none());
    }

    #[test]
    fn strict_blocks_sensitive_categories() {
        let m = GuardrailMode::Strict;
        assert!(!m.allows_category(categories::MEDICAL));
        assert!(!m.allows_category(categories::SECURITY));
        assert!(!m.allows_category(categories::LEGAL));
        assert!(!m.allows_category(categories::CREATIVE));
        assert!(!m.allows_category(categories::CODE));
        // Unknown categories pass through under strict mode
        // — strict isn't "block by default", it's "block the
        // documented sensitive set".
        assert!(m.allows_category("travel_query"));
    }

    #[test]
    fn balanced_allows_all_documented_categories() {
        let m = GuardrailMode::Balanced;
        for c in [
            categories::MEDICAL,
            categories::SECURITY,
            categories::LEGAL,
            categories::CREATIVE,
            categories::CODE,
        ] {
            assert!(m.allows_category(c), "balanced should allow {c}");
        }
    }

    #[test]
    fn permissive_allows_everything_except_hard_stops() {
        let m = GuardrailMode::Permissive;
        for c in [
            categories::MEDICAL,
            categories::SECURITY,
            categories::LEGAL,
            categories::CREATIVE,
            categories::CODE,
            "anything_else",
        ] {
            assert!(m.allows_category(c));
        }
        // Injection check still on — that's a hard stop, not
        // a category gate.
        assert!(m.injection_check());
    }

    #[test]
    fn pii_policy_matches_mode() {
        assert_eq!(GuardrailMode::Strict.pii_policy(), PiiPolicy::Block);
        assert_eq!(GuardrailMode::Balanced.pii_policy(), PiiPolicy::Redact);
        assert_eq!(GuardrailMode::Permissive.pii_policy(), PiiPolicy::Allow);
    }

    #[test]
    fn injection_check_is_always_on() {
        for m in [
            GuardrailMode::Strict,
            GuardrailMode::Balanced,
            GuardrailMode::Permissive,
        ] {
            assert!(m.injection_check(), "{m:?}: injection check must be on");
        }
    }

    #[test]
    fn default_is_balanced() {
        assert_eq!(GuardrailMode::default(), GuardrailMode::Balanced);
    }
}
