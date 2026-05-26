//! AI-side guardrails — input checks, drift detection, mode
//! calibration, multi-agent handoff inspection, and the
//! red-team eval harness.
//!
//! Distinct from [`crate::nodes::memory::guard::MemoryGuard`]:
//! the memory guard runs at the *storage* boundary so a
//! poisoned record never lands on disk. The AI guardrails in
//! this module run at the *request* boundary — they inspect
//! every prompt before it reaches the model, and every
//! handoff payload before it crosses agents.
//!
//! Honest scope: pure substring + regex checks. There is no
//! LLM classifier, and a determined attacker can paraphrase
//! around the patterns. The guardrails are one defensive
//! layer; the operator should layer rate limits, policy
//! admit rules, and audit trails alongside.

pub mod drift;
pub mod eval;
pub mod handoff;
pub mod input;
pub mod mode;

pub use drift::{
    ChronicleEvent, DriftAction, DriftConfig, DriftDetector, DriftEmbedDispatcher,
    DriftEmbedDispatcherCell, DriftEmbedDispatcherHandle, MeshDriftEmbedDispatcher,
};
pub use eval::{EvalCase, EvalFailure, EvalReport, GuardrailEval};
pub use handoff::{HandoffAuditEvent, HandoffGuard, HandoffGuardResult};
pub use input::{InputGuardrail, InputGuardrailResult, PiiPolicy};
pub use mode::GuardrailMode;

/// Stable category tags content classification can attach to
/// a request. Public so the bridge / dashboard can surface
/// them in the audit view without re-declaring the vocab.
pub mod categories {
    pub const MEDICAL: &str = "medical_query";
    pub const SECURITY: &str = "security_query";
    pub const LEGAL: &str = "legal_query";
    pub const CREATIVE: &str = "creative_writing";
    pub const CODE: &str = "code_request";
}
