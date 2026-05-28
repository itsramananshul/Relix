//! RELIX-7.24 — spec-driven multi-agent planning.
//!
//! An operator writes a natural-language specification of
//! what they want to accomplish. The planner reads the spec,
//! reasons about which agents are available + what they can
//! do, produces a structured `Workflow`, validates it through
//! the existing [`crate::workflow`] engine, and (optionally)
//! executes it.
//!
//! The planner builds ON TOP of the workflow engine — it
//! generates [`crate::workflow::Workflow`] values that the
//! existing executor consumes. It does NOT replace or
//! duplicate workflow logic. The pipeline is:
//!
//! ```text
//! spec (string)
//!   │
//!   ▼  SpecParser::parse(spec)
//! PlanSpec { goal, constraints, success_criteria, preferred_agents, ... }
//!   │
//!   ▼  PlanGenerator::generate(spec, registry)
//! Workflow (validated)
//!   │
//!   ▼  workflow::execute(workflow, dispatcher, input)   [non-dry-run only]
//! WorkflowResult
//! ```
//!
//! Module layout:
//!
//! - [`registry`] — [`AgentCapabilityRegistry`] indexing every
//!   known agent peer + its declared capabilities.
//! - [`parser`] — [`SpecParser`] heuristic spec → `PlanSpec`.
//! - [`generator`] — [`PlanGenerator`] `PlanSpec` →
//!   validated [`crate::workflow::Workflow`].
//! - [`coordinator`] — coordinator-side `planning.*` cap
//!   handlers wiring the three above to the dispatch bridge.

pub mod approval;
pub mod conflict;
pub mod coordinator;
pub mod critic;
pub mod generator;
pub mod orchestrator;
pub mod parser;
pub mod registry;
pub mod verification;

pub use approval::{
    ApprovalError, ApprovalRecord, ApprovalStatus, ApprovalStore, ApprovalTarget,
    DEFAULT_APPROVAL_TIMEOUT_SECS, VerificationEntry, format_pending_notification,
};
pub use conflict::{
    ConflictKind, ConflictResolutionEntry, ConflictResolutionReport, ConflictResolver,
    ResolutionStrategy,
};
pub use coordinator::{planning_capability_descriptors, register, spawn_approval_expiry_sweep};
pub use critic::{CriticConfig, CriticLoop, CriticOutcome, CriticVerdict, PlanProducer};
pub use generator::{GenerateError, GeneratorOptions, PlanGenerator, PlanTopology};
pub use orchestrator::{
    Orchestrator, OrchestratorConfig, OrchestratorError, OrchestratorOutcome, SpecialistAssignment,
};
pub use parser::{
    DEFAULT_COMPLEXITY_THRESHOLD, PLAN_SPEC_VERSION, PlanSpec, SpecChange, SpecParser,
    SpecVerificationError,
};
pub use registry::{AgentCapabilityRegistry, AgentInfo, AgentMatch, CapabilityInfo};
pub use verification::{
    VerificationConfig, VerificationHarness, VerificationOutcome, VerificationStrategy,
    execute_with_verification, pick_strategy,
};

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `[planning]` config block carrying the orchestrator + critic
/// knobs. Both default in the orchestrator-on,
/// critic-on, coordinator-as-AI-peer state — fresh installs
/// get sensible multi-specialist planning out of the box. An
/// operator who wants the legacy single-agent path back sets
/// `enabled = false` (orchestrator) and `critic_enabled =
/// false` (critic).
///
/// The orchestrator + critic fields are flattened to keep the
/// TOML layout flat — operators write a single `[planning]`
/// table with all the knobs rather than nested
/// `[planning.orchestrator]` + `[planning.critic]` sub-tables.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct PlanningConfig {
    #[serde(flatten)]
    pub orchestrator: OrchestratorConfig,
    #[serde(flatten)]
    pub critic: CriticConfig,
    /// RELIX-7.24 Stage-4 human-in-the-loop approval gate.
    /// When `true`, every call to `planning.create_plan` that
    /// would have executed (i.e. `dry_run = false`) is held in
    /// the [`ApprovalStore`] in `pending` state until an
    /// operator calls `planning.approve_plan` or
    /// `planning.reject_plan`.
    #[serde(default)]
    pub require_approval: bool,
    /// Pending plans older than this in the approval store
    /// are auto-rejected (status `expired`) by the background
    /// sweep. Default 3600s (1 hour).
    #[serde(default = "default_approval_timeout_secs")]
    pub approval_timeout_secs: i64,
    /// SQLite path for the approval store + verification log.
    /// Absent → the controller derives a path under the
    /// coordinator's `[coordinator] db_path` directory.
    #[serde(default)]
    pub approval_db_path: Option<PathBuf>,
    /// Channel targets for the pending-plan notification
    /// fan-out. Empty list → operators get a tracing-log
    /// entry only.
    #[serde(default)]
    pub approval_targets: Vec<ApprovalTarget>,
    /// RELIX-7.24 Stage-5 step-level verification harness.
    /// Flattened so operators write `verify_steps = true /
    /// verifier_agent = ... / required_steps = [...]` at the
    /// top of `[planning]` alongside the other knobs.
    #[serde(flatten)]
    pub verification: VerificationConfig,
}

fn default_approval_timeout_secs() -> i64 {
    DEFAULT_APPROVAL_TIMEOUT_SECS
}
