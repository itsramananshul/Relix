//! RELIX-7.19 — per-step confidence scoring + fallback.
//!
//! Every capability invocation that flows through the
//! [`crate::dispatch::DispatchBridge`] can be scored on a
//! 0.0–1.0 confidence scale. Operators wire fallback policies
//! (retry / escalate / safe_default / alert / abort) keyed by
//! capability name; the engine consults the scorer's verdict
//! and the per-cap [`FallbackPolicy`] to decide what happens
//! next. SOL flows can read the most recent score via the
//! `last_confidence()` builtin (see
//! [`crate::sol::vm::VM::set_last_confidence`]).
//!
//! Module layout:
//!
//! - [`config`] — `[confidence]` + `[[confidence.policies]]`
//!   TOML schema.
//! - [`scorer`] — [`ConfidenceScorer`] with rolling-window
//!   error-rate + latency tracking per `(agent, method)`.
//! - [`fallback`] — [`FallbackEngine`] + [`FallbackAction`] +
//!   glob-matched [`FallbackPolicy`] lookup.
//! - [`cell`] — [`LastConfidenceCell`] — a lock-free shared
//!   slot the dispatcher updates and the SOL VM reads via the
//!   `last_confidence()` builtin.
//! - [`coordinator`] — coordinator-side `confidence.*` cap
//!   handlers.

pub mod cell;
pub mod config;
pub mod coordinator;
pub mod fallback;
pub mod scorer;
pub mod self_consistency;

pub use cell::LastConfidenceCell;
pub use config::{
    ConfidenceConfig, ConfidencePolicy, ConfidenceWeights, FallbackActionConfig,
    confidence_capability_descriptors,
};
pub use coordinator::register;
pub use fallback::{ActionVerdict, FallbackAction, FallbackEngine};
pub use scorer::{ConfidenceScore, ConfidenceScorer, HistorySnapshot, ScoringInputs};
pub use self_consistency::{
    SampleEvaluation, SelfConsistencyConfig, SelfConsistencyOutcome, SelfConsistencyStats,
    SelfConsistencyStatsSnapshot, cosine_similarity, evaluate_samples, extract_core_answer,
};
