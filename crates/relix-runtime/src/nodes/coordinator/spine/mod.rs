//! The Guild **work-object spine** above the Brief: **Mandates**
//! and **Campaigns** (Phase 1). The Brief itself is the evolved
//! coordinator Task; this module adds the durable objects a
//! Brief links *up* to — Mandate (the "why") and Campaign (the
//! workstream) — each tenant-scoped (a Guild is the
//! product-facing name for a tenant).
//!
//! See `docs/relix-lexicon.md` for the naming: Mandate = the old
//! "Goal", Campaign = the old "Project". Kept self-contained so
//! the spine objects land without touching the existing Task
//! ledger.

pub mod handlers;
pub mod store;

pub use store::{
    Campaign, Mandate, OrchestrationRun, OrchestrationRunRecord, SpineStore, SpineStoreError,
    TeamPlan, TeamPlanRecord,
};
