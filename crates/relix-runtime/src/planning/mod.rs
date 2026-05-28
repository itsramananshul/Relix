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

pub mod coordinator;
pub mod generator;
pub mod orchestrator;
pub mod parser;
pub mod registry;

pub use coordinator::{planning_capability_descriptors, register};
pub use generator::{GenerateError, GeneratorOptions, PlanGenerator, PlanTopology};
pub use orchestrator::{
    Orchestrator, OrchestratorConfig, OrchestratorError, OrchestratorOutcome, SpecialistAssignment,
};
pub use parser::{DEFAULT_COMPLEXITY_THRESHOLD, PlanSpec, SpecParser};
pub use registry::{AgentCapabilityRegistry, AgentInfo, AgentMatch, CapabilityInfo};
