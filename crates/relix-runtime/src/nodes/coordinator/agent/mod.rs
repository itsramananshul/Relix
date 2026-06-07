//! Agent employee permission model — coordinator side.
//!
//! Stores agent profiles, approval requests, and standing
//! approvals in SQLite (sharing the coordinator's database).
//! Capability handlers expose CRUD + approval-management
//! surfaces; the admission gate (`crate::admission::agent_gate`)
//! reads from these tables on every inbound call.
//!
//! Six tables:
//!
//! - `agent_profiles`     — Phase 1+2 — one row per agent.
//! - `approval_requests`  — Phase 4 — one row per pending /
//!   decided approval.
//! - `standing_approvals` — Phase 5 — time-bounded categorical
//!   pre-approvals.

pub mod action_center;
pub mod handlers;
pub mod keys;
pub mod prime;
pub mod prime_deliberation;
pub mod prime_driver;
pub mod prime_orchestration;
pub mod prime_plan;
pub mod prime_plan_package;
pub mod prime_priority;
pub mod prime_strategy;
pub mod store;

pub use keys::{
    KeyVerdict, assign_verdict, configure_verdict, manage_verdict, secret_allowed, spawn_verdict,
};
pub use store::{
    AgentGateView, AgentProfile, AgentSnapshot, AgentStore, AgentStoreError, ApprovalRecord,
    ApprovalStatus, StandingApproval,
};
