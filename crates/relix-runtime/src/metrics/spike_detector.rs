//! GAP 22 Feature 2 follow-up — persistent rolling-baseline +
//! spike-detection scheduler.
//!
//! Runs every `interval_secs` (default 5 minutes). On each tick:
//!
//! 1. **Compute baselines.** For every model active in the last
//!    `baseline_window_mins`, capture a [`CostBaselineWindow`].
//!    For every agent active in the same window, capture an
//!    [`AskHumanRateWindow`]. Both rows persist to the
//!    [`CostBaselineStore`].
//! 2. **Check spikes.** Compare each model's just-computed average
//!    per-call cost against the rolling 24h baseline (read from
//!    the store). When the new window's average exceeds
//!    `spike_multiplier * baseline_avg`, archive a
//!    [`CostSpikeRecord`] AND fire a `CostSpikeAlert` through the
//!    configured [`AlertSink`].
//! 3. **Check drift.** Same logic, but on ask-human-rate per agent:
//!    when `recent_rate > baseline_rate + drift_threshold`, fire
//!    an `AskHumanRateDriftAlert`.
//! 4. **Purge.** Drop rows older than `retention_days`.
//!
//! This module is additive — the existing [`super::alert::AlertEngine`]
//! evaluator keeps running on its own poll cycle. The two paths
//! complement each other: the AlertEngine fires off the live metrics
//! store on its own tick; the detector here builds the durable history
//! the operator graphs against. They DON'T duplicate alert dispatch
//! because the detector uses its own dedup key prefix `"baseline:"` on
//! the AlertEvent's `agent` field — the engine's dedup keys never
//! collide.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::alert::{ActiveAlert, AlertEvent, AlertKind, AlertSeverity, AlertSink};
use super::collector::now_ms;
use super::cost_baseline::{
    AskHumanRateWindow, CostBaselineStore, CostBaselineWindow, CostSpikeRecord,
};
use super::query::{MetricsQuery, MetricsQueryError, percentile};
use super::store::MetricsStoreError;

/// `[metrics.cost_alerts]` config block.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CostAlertsConfig {
    /// Master switch. `false` keeps the detector dormant.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Width of each rolling window, in minutes. Default 60
    /// (= one row per provider/agent per hour).
    #[serde(default = "default_baseline_window_mins")]
    pub baseline_window_mins: u32,
    /// How often the detector ticks, in seconds. Default 300
    /// (= 5 minutes). The detector still writes one row per
    /// `baseline_window_mins`; the tick interval is just how
    /// often the computation runs (idempotent by id).
    #[serde(default = "default_tick_interval_secs")]
    pub tick_interval_secs: u64,
    /// Fire a CostSpikeAlert when the latest window's average
    /// per-call cost exceeds `spike_multiplier * baseline_avg`.
    /// Default 2.0 (= 2×).
    #[serde(default = "default_spike_multiplier")]
    pub spike_multiplier: f64,
    /// Fire an AskHumanRateDriftAlert when the latest window's
    /// ask-human rate exceeds the rolling baseline by
    /// `drift_threshold` (absolute). Default 0.3 (= 30%).
    #[serde(default = "default_drift_threshold")]
    pub drift_threshold: f64,
    /// Drop rows older than this many days on every tick.
    /// Default 7.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// Optional override for the SQLite path. When absent the
    /// coordinator derives it as `cost_baselines.db` next to
    /// the existing `metrics.db`.
    #[serde(default)]
    pub db_path: Option<std::path::PathBuf>,
    /// PART 4: absolute hourly spend cap, USD. When the
    /// rolling-hour cost across every model crosses this
    /// number, the collector fires a CostAlert with cause
    /// `absolute_hourly_cap_exceeded` — independent of any
    /// statistical baseline. `None` disables the cap.
    /// Default: $50/hour.
    #[serde(default = "default_absolute_hourly_cap_usd")]
    pub absolute_hourly_cap_usd: Option<f64>,
    /// PART 4: absolute daily spend cap, USD. Same shape as
    /// the hourly cap with a 24-hour rolling window. Default
    /// $500/day.
    #[serde(default = "default_absolute_daily_cap_usd")]
    pub absolute_daily_cap_usd: Option<f64>,
    /// PART 4: absolute per-request cap, USD. Checked BEFORE
    /// dispatch against the estimated cost and AFTER dispatch
    /// against the actual `cost_micros`. Default $5/request.
    #[serde(default = "default_absolute_per_request_cap_usd")]
    pub absolute_per_request_cap_usd: Option<f64>,
}

impl Default for CostAlertsConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            baseline_window_mins: default_baseline_window_mins(),
            tick_interval_secs: default_tick_interval_secs(),
            spike_multiplier: default_spike_multiplier(),
            drift_threshold: default_drift_threshold(),
            retention_days: default_retention_days(),
            db_path: None,
            absolute_hourly_cap_usd: default_absolute_hourly_cap_usd(),
            absolute_daily_cap_usd: default_absolute_daily_cap_usd(),
            absolute_per_request_cap_usd: default_absolute_per_request_cap_usd(),
        }
    }
}

fn default_enabled() -> bool {
    false
}
fn default_baseline_window_mins() -> u32 {
    60
}
fn default_tick_interval_secs() -> u64 {
    300
}
fn default_spike_multiplier() -> f64 {
    2.0
}
fn default_drift_threshold() -> f64 {
    0.3
}
fn default_retention_days() -> u32 {
    7
}

fn default_absolute_hourly_cap_usd() -> Option<f64> {
    Some(50.0)
}

fn default_absolute_daily_cap_usd() -> Option<f64> {
    Some(500.0)
}

fn default_absolute_per_request_cap_usd() -> Option<f64> {
    Some(5.0)
}

/// Per-tick summary the detector returns. Used by the spawn-task
/// for tracing and by the unit tests to assert behaviour.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DetectorTick {
    pub cost_baselines_inserted: u32,
    pub ask_human_baselines_inserted: u32,
    pub spikes_fired: u32,
    pub drifts_fired: u32,
    pub purged: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SpikeDetectorError {
    #[error("spike detector: metrics query: {0}")]
    Query(#[from] MetricsQueryError),
    #[error("spike detector: metrics store: {0}")]
    Store(#[from] MetricsStoreError),
    #[error("spike detector: baseline store: {0}")]
    Baseline(#[from] super::cost_baseline::BaselineStoreError),
}

/// Periodic cost-baseline + spike detector. Cheap to clone — holds
/// only Arcs of the dependencies.
#[derive(Clone)]
pub struct CostSpikeDetector {
    cfg: Arc<CostAlertsConfig>,
    query: MetricsQuery,
    store: CostBaselineStore,
    sink: AlertSink,
}

impl CostSpikeDetector {
    pub fn new(
        cfg: CostAlertsConfig,
        query: MetricsQuery,
        store: CostBaselineStore,
        sink: AlertSink,
    ) -> Self {
        Self {
            cfg: Arc::new(cfg),
            query,
            store,
            sink,
        }
    }

    pub fn config(&self) -> &CostAlertsConfig {
        &self.cfg
    }

    pub fn store(&self) -> &CostBaselineStore {
        &self.store
    }

    /// Run one full tick: compute baselines, evaluate spikes +
    /// drifts, purge old rows. Returns a [`DetectorTick`]
    /// describing what happened. Pure aside from the persistence
    /// + alert dispatch.
    pub fn tick(&self) -> Result<DetectorTick, SpikeDetectorError> {
        let mut t = DetectorTick::default();
        if !self.cfg.enabled {
            return Ok(t);
        }
        let now = now_ms();
        let window_ms = (self.cfg.baseline_window_mins as i64).max(1) * 60_000;
        let window_start = now - window_ms;
        let window_hours_for_query = ((self.cfg.baseline_window_mins as f64) / 60.0)
            .ceil()
            .max(1.0) as u32;

        // ── Per-model cost baselines + spike detection ──
        let models = self.query.list_models(window_hours_for_query)?;
        for model in &models {
            let window = self.compute_cost_window(model, window_start, now)?;
            if window.invocation_count == 0 {
                continue;
            }
            // Compute the baseline BEFORE inserting the new window
            // so the new row doesn't pollute its own comparison. On
            // the first tick (no history) the baseline falls back
            // to this tick's average — by definition not a spike,
            // which is the right behaviour.
            let baseline_avg = self
                .store
                .baseline_avg_micros(model, 24)?
                .unwrap_or(window.avg_cost_micros_per_call);
            self.store.insert_cost_baseline(&window)?;
            t.cost_baselines_inserted += 1;
            let multiplier = self.cfg.spike_multiplier.max(1.0);
            let crossed = baseline_avg > 0
                && (window.avg_cost_micros_per_call as f64) > (baseline_avg as f64) * multiplier;
            if crossed {
                let ratio = (window.avg_cost_micros_per_call as f64) / (baseline_avg as f64);
                let rec = CostSpikeRecord {
                    id: format!("spike:{}:{}", model, now),
                    provider: model.clone(),
                    current_avg_micros: window.avg_cost_micros_per_call,
                    baseline_avg_micros: baseline_avg,
                    spike_ratio: ratio,
                    window_start_ms: window.window_start_ms,
                    window_end_ms: window.window_end_ms,
                    created_at_ms: now,
                };
                self.store.insert_spike(&rec)?;
                self.fire_cost_spike(model, &rec);
                t.spikes_fired += 1;
            }
        }

        // ── Per-agent ask-human-rate baselines + drift detection ──
        let agents = self.query.list_agents(window_hours_for_query)?;
        for a in &agents {
            let window = self.compute_ask_human_window(&a.agent, window_start, now)?;
            if window.total_invocations == 0 {
                continue;
            }
            // Same ordering as the cost-spike path: compute the
            // 24h baseline BEFORE inserting the new window so the
            // new row doesn't dilute its own comparison.
            let baseline_rate = self
                .store
                .baseline_ask_human_rate(&a.agent, 24)?
                .unwrap_or(window.ask_human_rate);
            self.store.insert_ask_human_baseline(&window)?;
            t.ask_human_baselines_inserted += 1;
            let delta = window.ask_human_rate - baseline_rate;
            if delta > self.cfg.drift_threshold {
                self.fire_ask_human_drift(&a.agent, &window, baseline_rate, delta);
                t.drifts_fired += 1;
            }
        }

        // ── Retention ──
        t.purged = self.store.purge_older_than(self.cfg.retention_days)?;
        Ok(t)
    }

    fn compute_cost_window(
        &self,
        model: &str,
        window_start: i64,
        window_end: i64,
    ) -> Result<CostBaselineWindow, SpikeDetectorError> {
        // Pull per-call costs from the metrics store. The query
        // method is bounded by the (model, timestamp) index so it
        // stays sub-100ms even on a busy node.
        let rows: Vec<u64> = self.query.store().with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT cost_micros FROM metrics_invocations
                     WHERE model = ?1
                       AND timestamp_ms >= ?2
                       AND timestamp_ms < ?3
                       AND cost_micros IS NOT NULL",
            )?;
            let it = stmt.query_map(rusqlite::params![model, window_start, window_end], |r| {
                r.get::<_, Option<i64>>(0)
            })?;
            let mut v: Vec<u64> = Vec::new();
            for r in it {
                if let Some(n) = r? {
                    v.push(n.max(0) as u64);
                }
            }
            Ok(v)
        })?;
        let invocation_count = rows.len() as u64;
        let total_cost: u64 = rows.iter().copied().sum();
        let avg = total_cost.checked_div(invocation_count).unwrap_or(0);
        let p95 = if rows.is_empty() {
            0
        } else {
            let mut sorted = rows.clone();
            percentile(&mut sorted, 95.0)
        };
        Ok(CostBaselineWindow {
            id: format!("cb:{}:{}", model, window_end),
            provider: model.to_string(),
            window_start_ms: window_start,
            window_end_ms: window_end,
            total_cost_micros: total_cost,
            invocation_count,
            avg_cost_micros_per_call: avg,
            p95_cost_micros: p95,
            created_at_ms: now_ms(),
        })
    }

    fn compute_ask_human_window(
        &self,
        agent: &str,
        window_start: i64,
        window_end: i64,
    ) -> Result<AskHumanRateWindow, SpikeDetectorError> {
        let (approval, total): (u64, u64) = self.query.store().with_conn(|c| {
            c.query_row(
                "SELECT
                         SUM(CASE WHEN error_kind = 'APPROVAL_REQUIRED' THEN 1 ELSE 0 END),
                         COUNT(*)
                     FROM metrics_invocations
                     WHERE agent_name = ?1
                       AND timestamp_ms >= ?2
                       AND timestamp_ms < ?3",
                rusqlite::params![agent, window_start, window_end],
                |r| {
                    let a: Option<i64> = r.get(0)?;
                    let t: i64 = r.get(1)?;
                    Ok((a.unwrap_or(0).max(0) as u64, t.max(0) as u64))
                },
            )
        })?;
        let rate = if total > 0 {
            approval as f64 / total as f64
        } else {
            0.0
        };
        Ok(AskHumanRateWindow {
            id: format!("ahb:{}:{}", agent, window_end),
            agent: agent.to_string(),
            window_start_ms: window_start,
            window_end_ms: window_end,
            total_invocations: total,
            ask_human_count: approval,
            ask_human_rate: rate,
            created_at_ms: now_ms(),
        })
    }

    fn fire_cost_spike(&self, model: &str, rec: &CostSpikeRecord) {
        let message = format!(
            "{model}: avg cost {curr}µ¢/call vs 24h baseline {base}µ¢/call \
             (ratio {ratio:.2}×); window {ws}..{we}",
            curr = rec.current_avg_micros,
            base = rec.baseline_avg_micros,
            ratio = rec.spike_ratio,
            ws = rec.window_start_ms,
            we = rec.window_end_ms,
        );
        let active = ActiveAlert {
            agent: format!("baseline:model:{}", model),
            kind: AlertKind::ProviderCostSpike,
            severity: AlertSeverity::Warning,
            triggered_at_ms: rec.created_at_ms,
            threshold: rec.baseline_avg_micros as f64 * self.cfg.spike_multiplier,
            actual: rec.current_avg_micros as f64,
            message,
            method: None,
        };
        self.sink.deliver(&AlertEvent::Fired(active));
    }

    fn fire_ask_human_drift(
        &self,
        agent: &str,
        window: &AskHumanRateWindow,
        baseline_rate: f64,
        delta: f64,
    ) {
        let message = format!(
            "{agent}: ask-human rate {curr:.2}% over the last \
             {win} mins ({approved}/{total} APPROVAL_REQUIRED); \
             24h baseline {base:.2}%; delta +{delta:.2}",
            curr = window.ask_human_rate * 100.0,
            win = (window.window_end_ms - window.window_start_ms) / 60_000,
            approved = window.ask_human_count,
            total = window.total_invocations,
            base = baseline_rate * 100.0,
            delta = delta,
        );
        let active = ActiveAlert {
            agent: format!("baseline:agent:{}", agent),
            kind: AlertKind::AskHumanRateDrift,
            severity: AlertSeverity::Warning,
            triggered_at_ms: window.created_at_ms,
            threshold: baseline_rate + self.cfg.drift_threshold,
            actual: window.ask_human_rate,
            message,
            method: None,
        };
        self.sink.deliver(&AlertEvent::Fired(active));
    }

    /// Spawn the detector on a background tokio task that ticks every
    /// `tick_interval_secs`. The handle is detached; the detector is
    /// supposed to outlive the controller process. Returns
    /// immediately.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        let interval = Duration::from_secs(self.cfg.tick_interval_secs.max(60));
        tokio::spawn(async move {
            tracing::info!(
                interval_secs = interval.as_secs(),
                baseline_window_mins = self.cfg.baseline_window_mins,
                spike_multiplier = self.cfg.spike_multiplier,
                drift_threshold = self.cfg.drift_threshold,
                retention_days = self.cfg.retention_days,
                "cost-spike detector online (GAP 22 Feature 2 baseline store)"
            );
            let mut ticker = tokio::time::interval(interval);
            // First tick fires immediately; suppress to avoid a
            // duplicate evaluation at startup if AlertEngine ran
            // concurrently.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                match self.tick() {
                    Ok(out) => {
                        tracing::debug!(
                            cost_baselines = out.cost_baselines_inserted,
                            ask_human = out.ask_human_baselines_inserted,
                            spikes = out.spikes_fired,
                            drifts = out.drifts_fired,
                            purged = out.purged,
                            "cost-spike detector tick",
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "cost-spike detector tick failed");
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::alert::{AlertDeliver, LoggingAlertSink};
    use super::super::collector::now_ms;
    use super::super::store::MetricsStore;
    use super::super::types::InvocationMetric;
    use super::*;
    use std::sync::Mutex;

    fn metric(
        agent: &str,
        method: &str,
        ts: i64,
        success: bool,
        error_kind: Option<&str>,
        cost: Option<u64>,
        model: Option<&str>,
    ) -> InvocationMetric {
        InvocationMetric {
            agent_name: agent.into(),
            tenant_id: "default".into(),
            peer_alias: "p".into(),
            method: method.into(),
            timestamp_ms: ts,
            latency_ms: 100,
            success,
            error_kind: error_kind.map(|s| s.into()),
            token_count: Some(100),
            cost_micros: cost,
            input_bytes: 100,
            output_bytes: 100,
            model: model.map(|s| s.into()),
            confidence_score: None,
            routing_tier: None,
            request_id: None,
        }
    }

    #[derive(Default)]
    struct CountingSink {
        seen: Mutex<Vec<AlertEvent>>,
    }

    impl AlertDeliver for CountingSink {
        fn deliver(&self, e: &AlertEvent) {
            self.seen.lock().unwrap().push(e.clone());
        }
    }

    fn build_detector(
        thresholds: CostAlertsConfig,
    ) -> (CostSpikeDetector, MetricsStore, Arc<CountingSink>) {
        let metrics_store = MetricsStore::in_memory().unwrap();
        let query = MetricsQuery::new(metrics_store.clone());
        let baseline_store = CostBaselineStore::in_memory().unwrap();
        let sink = Arc::new(CountingSink::default());
        // AlertSink takes ownership of a sink type; we wrap our
        // counting sink in a delegating shim because AlertSink::new
        // moves the inner value.
        let inner: Arc<dyn AlertDeliver> = sink.clone();
        let alert_sink = AlertSinkBuilder(inner).build();
        let detector = CostSpikeDetector::new(thresholds, query, baseline_store, alert_sink);
        (detector, metrics_store, sink)
    }

    // Tiny shim that lets the test re-use an existing Arc<dyn
    // AlertDeliver> behind the AlertSink wrapper. AlertSink::new
    // takes ownership of an inner value, so we use a small thunk
    // that forwards `.deliver`.
    struct AlertSinkBuilder(Arc<dyn AlertDeliver>);
    impl AlertSinkBuilder {
        fn build(self) -> AlertSink {
            AlertSink::new(SharedAlertSink(self.0))
        }
    }
    struct SharedAlertSink(Arc<dyn AlertDeliver>);
    impl AlertDeliver for SharedAlertSink {
        fn deliver(&self, e: &AlertEvent) {
            self.0.deliver(e);
        }
    }

    fn detector_cfg() -> CostAlertsConfig {
        CostAlertsConfig {
            enabled: true,
            baseline_window_mins: 60,
            tick_interval_secs: 300,
            spike_multiplier: 2.0,
            drift_threshold: 0.3,
            retention_days: 7,
            db_path: None,
            ..Default::default()
        }
    }

    #[test]
    fn disabled_tick_is_a_noop() {
        let cfg = CostAlertsConfig {
            enabled: false,
            ..detector_cfg()
        };
        let (det, _, sink) = build_detector(cfg);
        let out = det.tick().unwrap();
        assert_eq!(out, DetectorTick::default());
        assert_eq!(sink.seen.lock().unwrap().len(), 0);
    }

    #[test]
    fn baseline_inserted_with_avg_and_p95() {
        let (det, ms, _sink) = build_detector(detector_cfg());
        let now = now_ms();
        // 10 calls with varied costs in the last minute.
        for cost in [1000, 1000, 1000, 1000, 1000, 2000, 2000, 2000, 3000, 5000] {
            ms.insert(&metric(
                "alice",
                "ai.chat",
                now - 30_000,
                true,
                None,
                Some(cost),
                Some("gpt-4o-mini"),
            ))
            .unwrap();
        }
        let out = det.tick().unwrap();
        assert_eq!(out.cost_baselines_inserted, 1);
        let rows = det
            .store()
            .recent_cost_baselines(Some("gpt-4o-mini"), 10)
            .unwrap();
        assert_eq!(rows.len(), 1);
        // avg = (1000*5 + 2000*3 + 3000 + 5000) / 10 = 19_000/10 = 1900.
        assert_eq!(rows[0].avg_cost_micros_per_call, 1_900);
        assert_eq!(rows[0].p95_cost_micros, 5_000);
        assert_eq!(rows[0].invocation_count, 10);
    }

    #[test]
    fn spike_fires_when_recent_avg_exceeds_multiplier_x_baseline() {
        let (det, ms, sink) = build_detector(detector_cfg());
        let now = now_ms();
        // Build a baseline window in the store: avg = 1000 micros.
        for _ in 0..10 {
            ms.insert(&metric(
                "alice",
                "ai.chat",
                now - 30_000,
                true,
                None,
                Some(1_000),
                Some("gpt-4o-mini"),
            ))
            .unwrap();
        }
        // First tick: writes a 1000-avg baseline.
        det.tick().unwrap();
        // Insert a spike: 10 calls at 5000 micros each (5× the baseline).
        // Stamp at `now - 1` so the spike rows fall strictly INSIDE
        // tick #2's window — the cost-window SQL is half-open
        // (`timestamp_ms < window_end`), and on a fast host tick #2's
        // internal `now` can land in the same millisecond as the
        // test's `now`. Using `now - 1` keeps the assertion
        // deterministic regardless of scheduler jitter.
        for _ in 0..10 {
            ms.insert(&metric(
                "alice",
                "ai.chat",
                now - 1,
                true,
                None,
                Some(5_000),
                Some("gpt-4o-mini"),
            ))
            .unwrap();
        }
        // Second tick: avg = (10*1000 + 10*5000)/20 = 3000.
        // Baseline now includes the previous baseline (1000); 3000 > 2*1000 — fires.
        let out = det.tick().unwrap();
        assert_eq!(out.cost_baselines_inserted, 1);
        assert_eq!(out.spikes_fired, 1);
        let history = det.store().recent_spike_history(10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].provider, "gpt-4o-mini");
        let fired = sink.seen.lock().unwrap().clone();
        assert!(
            fired.iter().any(
                |e| matches!(e, AlertEvent::Fired(a) if a.kind == AlertKind::ProviderCostSpike)
            )
        );
    }

    #[test]
    fn spike_does_not_fire_when_recent_below_multiplier() {
        let (det, ms, sink) = build_detector(detector_cfg());
        let now = now_ms();
        // Baseline: avg = 1000 micros.
        for _ in 0..10 {
            ms.insert(&metric(
                "alice",
                "ai.chat",
                now - 30_000,
                true,
                None,
                Some(1_000),
                Some("gpt-4o-mini"),
            ))
            .unwrap();
        }
        det.tick().unwrap();
        // Next: 10 calls at 1500 (= 1.5×, below 2× threshold).
        for _ in 0..10 {
            ms.insert(&metric(
                "alice",
                "ai.chat",
                now,
                true,
                None,
                Some(1_500),
                Some("gpt-4o-mini"),
            ))
            .unwrap();
        }
        let out = det.tick().unwrap();
        assert_eq!(out.spikes_fired, 0);
        assert_eq!(sink.seen.lock().unwrap().len(), 0);
    }

    #[test]
    fn drift_fires_when_recent_rate_exceeds_baseline_plus_threshold() {
        let (det, ms, sink) = build_detector(detector_cfg());
        let now = now_ms();
        // Pre-seed the persistent baseline store directly with a
        // historical 5% baseline. This models tick N+24h: the
        // store has 24h of low-rate baselines and the current
        // metrics window shows a burst that the detector should
        // catch as drift.
        for j in 0..5 {
            det.store()
                .insert_ask_human_baseline(&AskHumanRateWindow {
                    id: format!("seed:{j}"),
                    agent: "alice".into(),
                    window_start_ms: now - (j + 2) * 3_600_000,
                    window_end_ms: now - (j + 1) * 3_600_000,
                    total_invocations: 100,
                    ask_human_count: 5,
                    ask_human_rate: 0.05,
                    created_at_ms: now - (j + 1) * 3_600_000,
                })
                .unwrap();
        }
        // Recent metrics: 100 calls, 50 APPROVAL_REQUIRED → 50%.
        // Delta vs 5% baseline = 45pp, well over the 30pp
        // threshold.
        for i in 0..50i64 {
            ms.insert(&metric(
                "alice",
                "ai.chat",
                now - 30_000 - i,
                true,
                None,
                None,
                None,
            ))
            .unwrap();
        }
        for i in 0..50i64 {
            ms.insert(&metric(
                "alice",
                "ai.chat",
                now - 30_000 - 1_000 - i,
                false,
                Some("APPROVAL_REQUIRED"),
                None,
                None,
            ))
            .unwrap();
        }
        let out = det.tick().unwrap();
        assert!(out.drifts_fired >= 1, "drift not fired: {out:?}");
        let fired = sink.seen.lock().unwrap().clone();
        assert!(
            fired.iter().any(
                |e| matches!(e, AlertEvent::Fired(a) if a.kind == AlertKind::AskHumanRateDrift)
            )
        );
    }

    #[test]
    fn drift_does_not_fire_for_small_change() {
        let (det, ms, sink) = build_detector(detector_cfg());
        let now = now_ms();
        // Baseline 5%.
        for i in 0..95 {
            ms.insert(&metric(
                "alice",
                "ai.chat",
                now - 30_000 - i,
                true,
                None,
                None,
                None,
            ))
            .unwrap();
        }
        for i in 0..5 {
            ms.insert(&metric(
                "alice",
                "ai.chat",
                now - 30_000 - 100 - i,
                false,
                Some("APPROVAL_REQUIRED"),
                None,
                None,
            ))
            .unwrap();
        }
        det.tick().unwrap();
        // Recent: 10% (= delta of 5pp, below 30% threshold).
        for i in 0..90 {
            ms.insert(&metric("alice", "ai.chat", now + i, true, None, None, None))
                .unwrap();
        }
        for i in 0..10 {
            ms.insert(&metric(
                "alice",
                "ai.chat",
                now + 1_000 + i,
                false,
                Some("APPROVAL_REQUIRED"),
                None,
                None,
            ))
            .unwrap();
        }
        let out = det.tick().unwrap();
        assert_eq!(out.drifts_fired, 0);
        let fired = sink.seen.lock().unwrap().clone();
        assert!(
            fired.iter().all(
                |e| !matches!(e, AlertEvent::Fired(a) if a.kind == AlertKind::AskHumanRateDrift)
            )
        );
    }

    #[test]
    fn purge_runs_each_tick() {
        let (det, ms, _sink) = build_detector(detector_cfg());
        let now = now_ms();
        // Insert a metric so the tick actually does something.
        ms.insert(&metric(
            "alice",
            "ai.chat",
            now - 30_000,
            true,
            None,
            Some(1_000),
            Some("gpt-4o-mini"),
        ))
        .unwrap();
        det.tick().unwrap();
        // Backfill an 8-day-old baseline row directly.
        det.store()
            .insert_cost_baseline(&CostBaselineWindow {
                id: "old:row".into(),
                provider: "old".into(),
                window_start_ms: now - 8 * 86_400_000 - 60_000,
                window_end_ms: now - 8 * 86_400_000,
                total_cost_micros: 1,
                invocation_count: 1,
                avg_cost_micros_per_call: 1,
                p95_cost_micros: 1,
                created_at_ms: now - 8 * 86_400_000,
            })
            .unwrap();
        let out = det.tick().unwrap();
        // Default retention_days = 7, so the 8d row is purged.
        assert!(out.purged >= 1);
    }

    #[test]
    fn dispatch_through_logging_sink_does_not_panic() {
        // Smoke test: the LoggingAlertSink path is wired by default
        // in deployments without channel sinks. Make sure the
        // detector's dispatch shape is compatible.
        let metrics_store = MetricsStore::in_memory().unwrap();
        let query = MetricsQuery::new(metrics_store);
        let baseline_store = CostBaselineStore::in_memory().unwrap();
        let sink = AlertSink::new(LoggingAlertSink);
        let det = CostSpikeDetector::new(detector_cfg(), query, baseline_store, sink);
        // No data → nothing to do, but the tick should succeed.
        let out = det.tick().unwrap();
        assert_eq!(out.cost_baselines_inserted, 0);
    }
}
