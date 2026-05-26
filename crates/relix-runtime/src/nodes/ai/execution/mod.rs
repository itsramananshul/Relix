//! Execution layer — separates planning, policy, and
//! execution into three small, testable surfaces.
//!
//! The shape mirrors the standard agent-systems split:
//!
//! 1. **Planner** — parses a raw model reply into a
//!    structured [`ExecutionPlan`] and classifies its
//!    reversibility. Pure logic, no I/O.
//! 2. **Policy** — evaluates a plan against operator-
//!    configured rules (step caps, cost thresholds, the
//!    "allow irreversible?" switch). Returns
//!    [`PolicyVerdict`].
//! 3. **Executor** — a thin state machine that tracks which
//!    step is currently in flight and accepts the result of
//!    each step from the caller. Capture of evidence + the
//!    chronicle append happen here.
//!
//! Today's `handle_chat` runs the full split as a fast path:
//! parse the reply, evaluate policy, return the model text
//! when no irreversible / approval gates fire, or surface an
//! approval-request response otherwise. The richer multi-
//! step executor (tool dispatch, ActionGateway integration)
//! lands as `handle_chat` grows past single-turn replies.

pub mod executor;
pub mod planner;
pub mod policy;
pub mod tool_runner;

pub use executor::{EvidenceRecord, ExecutionState, Executor, StepResult};
pub use planner::{ExecutionPlan, PlanStep, Planner, Reversibility};
pub use policy::{PolicyEngine, PolicyVerdict};
pub use tool_runner::{ToolMeshDispatcher, dispatch_planner_tool_calls, structured_dispatch_error};
