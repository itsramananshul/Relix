//! GAP 16 — §7.29 Reasoning and Decision Engine.
//!
//! Four sub-components ride here:
//!
//! - [`classifier`] / [`tier_router`] — Component 1, smart
//!   model routing. The classifier turns an incoming chat
//!   request into a `ReasoningTier` (Simple / Medium /
//!   Complex). The tier router maps a tier onto the
//!   configured model id for that tier (operators set the
//!   mappings via `[reasoning.router.tiers]`).
//! - [`judge`] — Component 4, the 5-question judge model.
//!   Runs after a Tier-3 or irreversible call; counts flags;
//!   returns a verdict the dispatcher acts on.
//! - [`confidence_signals`] — Component 2, the three-signal
//!   real-confidence path (self-consistency + judge scan; the
//!   third signal, retrieval quality, is documented as a
//!   deferred follow-up because it needs per-call retrieval
//!   context the dispatcher doesn't carry today).
//! - [`belief`] — Component 3, the per-session belief state
//!   tracker. Sits alongside the four-layer memory store as
//!   a session-scoped working memory.
//!
//! The four sub-components are independent — operators can
//! turn each on or off via the `[reasoning.*]` config blocks.
//! When everything is disabled (the default), the AI handler
//! runs byte-for-byte as it did pre-7.29.

pub mod belief;
pub mod classifier;
pub mod confidence_signals;
pub mod config;
pub mod judge;
pub mod tier_router;

pub use belief::{Belief, BeliefConflict, BeliefStore, BeliefStoreError};
pub use classifier::{ComplexityClassifier, ReasoningTier};
pub use confidence_signals::{SelfConsistencyOutcome, ThreeSignalConfidence, ThreeSignalScore};
pub use config::{
    BeliefConfig, JudgeConfig, ReasoningConfig, ReasoningRouterConfig, RouterTiers,
    SelfConsistencyConfig,
};
pub use judge::{JudgeAction, JudgeFlag, JudgeQuestion, JudgeVerdict};
pub use tier_router::TierRouter;
