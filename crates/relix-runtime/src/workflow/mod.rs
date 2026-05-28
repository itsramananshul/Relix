//! Multi-agent workflow engine — the foundation layer that
//! makes agent-to-agent coordination programmable.
//!
//! A workflow is a typed DAG of *agent steps*. Each step
//! dispatches a single capability call against a peer
//! identified by alias. Steps bind their result into named
//! output variables; later steps interpolate those variables
//! into their inputs via `{{<step>.output}}` markers (same
//! `{{name}}` syntax SOL uses).
//!
//! The engine ships in five concrete pieces:
//!
//! - [`ast`] — typed workflow definition (`Workflow`,
//!   `AgentSpec`, `FlowGraph`, `Edge`, `EdgeCondition`).
//! - [`parser`] — YAML → AST using saphyr's annotated tree
//!   (line + column on every schema error).
//! - [`validator`] — graph + variable + peer existence
//!   checks. Cycles in sequential flows, undefined variable
//!   references, edges pointing at non-existent agents,
//!   missing required fields — all surface as a
//!   `ValidationError` with the offending field name.
//! - [`executor`] — drives a parsed + validated workflow.
//!   Sequential, conditional, and parallel modes; per-step
//!   trace; structured `WorkflowResult` on success or
//!   failure. Never panics, always returns a typed result.
//! - [`dispatcher`] — `WorkflowDispatcher` trait the
//!   executor calls to dispatch one capability call. Real
//!   impls wrap libp2p; tests use a recording stub.
//!
//! See `crates/relix-runtime/src/nodes/coordinator` for the
//! capability registration (`workflow.run` /
//! `workflow.list` / `workflow.status` / `workflow.validate`)
//! and the chronicle integration that turns each execution
//! into an auditable record.

pub mod ast;
pub mod chronicle;
pub mod coordinator;
pub mod dispatcher;
pub mod executor;
pub mod mesh_dispatcher;
pub mod parser;
pub mod store;
pub mod validator;

#[cfg(test)]
mod tests;

pub use ast::{AgentSpec, Edge, EdgeCondition, FlowGraph, Workflow};
pub use chronicle::{ChronicleError, ExecutionRecord, StepRecord, WorkflowChronicle};
pub use dispatcher::{DispatchError, DispatchResult, WorkflowDispatcher};
pub use executor::{
    CancellationFlag, ExecutionId, ExecutionStatus, ExecutionStep, ExecutionTrace, WorkflowEvent,
    WorkflowExecutor, WorkflowResult, execute, execute_with_cancellation, execute_with_events,
};
pub use mesh_dispatcher::{MeshWorkflowDispatcher, WorkflowDispatcherCell};
pub use parser::{ParseError, parse_str};
pub use store::{StoreError, WorkflowEntry, WorkflowStore};
pub use validator::{ValidationError, validate};
