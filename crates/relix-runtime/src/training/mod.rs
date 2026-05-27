//! RELIX-7.15 — training data pipeline.
//!
//! End-to-end pipeline for capturing AI agent interactions and
//! turning them into fine-tuning datasets:
//!
//! - [`types`]       — `InteractionRecord` + `ToolCallRecord` +
//!   aggregate stats payloads.
//! - [`store`]       — append-only `training.sqlite` store
//!   with pagination + filter + export-ordering query paths.
//! - [`recorder`]    — non-blocking `InteractionSink` the AI
//!   handler holds, plus the background drain + retention
//!   loops.
//! - [`scorer`]      — deterministic quality scorer (response
//!   length / latency / tool success / coherence / success).
//! - [`exporter`]    — OpenAI / Anthropic / generic / raw_json
//!   export engine.
//! - [`coordinator`] — coordinator-side capability
//!   registration (`training.*`).
//! - [`config`]      — `[training]` TOML schema.

pub mod config;
pub mod coordinator;
pub mod exporter;
pub mod pii;
pub mod recorder;
pub mod scorer;
pub mod store;
pub mod types;

pub use config::{TrainingConfig, default_export_dir, default_training_path};
pub use coordinator::{register, training_capability_descriptors};
pub use exporter::{ExportEngine, ExportError, ExportFilters, ExportFormat, ExportResult};
pub use pii::{PiiAnonymizer, PiiConfig, PiiDetector, PiiSpan, PiiStrategy, PiiType};
pub use recorder::{
    AgentTrainingPolicies, CollectingInteractionSink, InteractionRecorder, InteractionSink,
    NullInteractionSink, RecorderWorkerHandles, RetentionConfig, SpawnedRecorder, anonymize_record,
    apply_anonymizer,
};
pub use scorer::{
    QualityScorer, ScoreBreakdown, ScorerConfig, score_one, score_one_batch, spawn_scorer_loop,
};
pub use store::{ListFilters, TrainingStore, TrainingStoreError};
pub use types::{
    GroupedCount, InteractionId, InteractionRecord, InteractionSummary, ScoreDistribution,
    ToolCallRecord, TrainingStats,
};
