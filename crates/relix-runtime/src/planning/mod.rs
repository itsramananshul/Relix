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

pub mod conflict;
pub mod coordinator;
pub mod critic;
pub mod generator;
pub mod orchestrator;
pub mod parser;
pub mod registry;

pub use conflict::{
    ConflictKind, ConflictResolutionEntry, ConflictResolutionReport, ConflictResolver,
    ResolutionStrategy,
};
pub use coordinator::{planning_capability_descriptors, register};
pub use critic::{CriticConfig, CriticLoop, CriticOutcome, CriticVerdict, PlanProducer};
pub use generator::{GenerateError, GeneratorOptions, PlanGenerator, PlanTopology};
pub use orchestrator::{
    Orchestrator, OrchestratorConfig, OrchestratorError, OrchestratorOutcome, SpecialistAssignment,
};
pub use parser::{DEFAULT_COMPLEXITY_THRESHOLD, PlanSpec, SpecParser};
pub use registry::{AgentCapabilityRegistry, AgentInfo, AgentMatch, CapabilityInfo};

use serde::{Deserialize, Serialize};

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
}
