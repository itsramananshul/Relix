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
pub mod budget;
pub mod budget_coordinator;
pub mod collector;
pub mod config;
pub mod coordinator;
pub mod cost_baseline;
pub mod observability;
pub mod pricing;
pub mod query;
pub mod spike_detector;
pub mod store;
pub mod types;

pub use alert::{
    ActiveAlert, AlertDeliver, AlertEngine, AlertEvent, AlertKind, AlertSeverity, AlertSink,
    AlertThresholds, LoggingAlertSink,
};
pub use alert_delivery::{
    AlertChronicle, AlertChronicleRow, AlertDeliveryConfig, AlertMeshCell, AlertMeshContext,
    AlertTarget, ChronicleAlertSink, ChronicleError as AlertChronicleError, CompositeAlertSink,
    MultiChannelAlertSink,
};
pub use budget::{
    AgentBudget, AgentStatusRow, BudgetAction, BudgetBreach, BudgetConfig, BudgetDecision,
    BudgetEnforcer, BudgetStatus, DeploymentBudget, DeploymentStatusRow, Window as BudgetWindow,
    parse_window as parse_budget_window,
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
pub use types::{AiProviderSignalsHint, AiSelfConsistencyHint, AiUsageHint, InvocationMetric};
