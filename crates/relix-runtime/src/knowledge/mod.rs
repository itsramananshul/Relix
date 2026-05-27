//! RELIX-7.16 — agent-to-agent knowledge transfer.
//!
//! Built ON TOP of the existing Layer 3 observation surface in
//! [`crate::nodes::memory`]. Operators tag observations with a
//! [`SharePolicy`](crate::nodes::memory::schema::SharePolicy);
//! the [`KnowledgeService`] copies them between agents under
//! a [`TrustChecker`] policy gate. Five coordinator
//! capabilities + a periodic [`AutoShareTask`] back the
//! operator surface.
//!
//! The module is organised:
//!
//! - [`config`] — `[[knowledge.groups]]` TOML schema +
//!   resolver (`is_member`, `members_of`,
//!   `groups_for_agent`).
//! - [`trust`] — pre-insert validation: group membership +
//!   memory-poison detection + `min_quality_score` cap +
//!   per-agent observation-count cap.
//! - [`service`] — `KnowledgeService` carrying the
//!   `LayeredMemoryStore` + `KnowledgeConfig` + trust checker.
//!   `share` / `list_shared` / `group_broadcast` /
//!   `revoke` methods.
//! - [`autoshare`] — `AutoShareTask::spawn(...)`. 60s tick
//!   that walks each group's auto-share-tagged
//!   observations and propagates them through the trust
//!   gate.
//! - [`coordinator`] — `knowledge.*` dispatch handlers.
//! - [`chronicle`] — typed event payloads written to the
//!   coordinator's chronicle every time knowledge moves.

pub mod autoshare;
pub mod chronicle;
pub mod config;
pub mod coordinator;
pub mod quality_scorer;
pub mod remote;
pub mod service;
pub mod trust;

pub use autoshare::{
    AutoShareConfig, AutoShareLifetimeStats, AutoShareTask, AutoShareTickStats, LifetimeCounters,
};
pub use chronicle::{KnowledgeEvent, KnowledgeEventKind};
pub use config::{KnowledgeConfig, MemberNodeRoute, SharingGroup, sharing_group_descriptors};
pub use coordinator::{knowledge_capability_descriptors, register};
pub use quality_scorer::{
    MemoryQualityScorer, MemoryQualityScorerConfig, ScoreBreakdown as MemoryScoreBreakdown,
    format_quality_tag, score_one_batch as score_one_memory_batch, spawn_memory_quality_scorer,
};
pub use remote::{
    InMemoryRemoteDispatcher, LateBoundDispatcher, MeshKnowledgeDispatcher, MeshKnowledgeRouter,
    NullRemoteDispatcher, RemoteKnowledgeDispatcher, RemoteShareError, SignedSharePayload,
};
pub use service::{
    BroadcastResult, KnowledgeService, ListSharedFilter, ListSharedRow, RecallResult,
    RecallTargetSummary, RevokeResult, ShareError, ShareRejection, ShareRequest, ShareResult,
};
pub use trust::{RejectReason, TrustChecker};
