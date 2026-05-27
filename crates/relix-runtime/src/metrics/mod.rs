//! RELIX-7.11 Agent Performance Dashboard — per-agent
//! metrics collection, aggregation queries, and alerting.
//!
//! Architecture:
//!
//! - `types`     — the canonical `InvocationMetric` row + the
//!   optional `AiUsageHint` enrichment sidecar.
//! - `store`     — append-only SQLite store. WAL-mode, indexed
//!   on `(agent, timestamp)` and `(method, timestamp)`.
//! - `pricing`   — model → micro-USD price table for cost
//!   estimation. Defaults shipped; `[metrics.prices]` overrides.
//! - `collector` — async batching layer that owns the store +
//!   the drain + retention background tasks. Exposes a
//!   non-blocking `MetricsSink` trait the dispatch bridge holds.
//! - `query`     — read-side aggregation queries (per-agent /
//!   per-method summary, P50/P95/P99 latency, time-series
//!   bucketing).
//! - `alert`     — periodic threshold evaluator that fires
//!   alert events to the coordinator's chronicle + the
//!   configured channels.
//! - `coordinator` — coordinator-side capability registration
//!   for `metrics.agent_summary` / `method_breakdown` /
//!   `timeseries` / `alerts_active` / `cost_report`.
//! - `config`    — top-level `[metrics]` TOML schema.

pub mod alert;
pub mod alert_delivery;
pub mod collector;
pub mod config;
pub mod coordinator;
pub mod pricing;
pub mod query;
pub mod store;
pub mod types;

pub use alert::{ActiveAlert, AlertEngine, AlertEvent, AlertKind, AlertSeverity, AlertThresholds};
pub use alert_delivery::{
    AlertChronicle, AlertChronicleRow, AlertDeliveryConfig, AlertMeshCell, AlertMeshContext,
    AlertTarget, ChronicleAlertSink, ChronicleError as AlertChronicleError, CompositeAlertSink,
    MultiChannelAlertSink,
};
pub use collector::{
    MetricsCollector, MetricsSink, MetricsWorkerHandles, NullMetricsSink, RetentionConfig,
    SpawnedMetrics,
};
pub use config::{MetricsConfig, default_metrics_path};
pub use pricing::{ModelPrice, PriceTable, PriceTableConfig};
pub use query::{
    AgentSummary, MethodSummary, MetricsQuery, MetricsQueryError, TimeseriesBucket,
    TimeseriesQuery, percentile,
};
pub use store::{MetricsStore, MetricsStoreError};
pub use types::{AiUsageHint, InvocationMetric};
