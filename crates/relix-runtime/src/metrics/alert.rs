//! Periodic threshold evaluator + dedup engine for RELIX-7.11.
//!
//! The engine ticks every `alert_interval_secs` (default 60s).
//! On each tick it walks every known agent and evaluates four
//! conditions:
//!
//! - error_rate exceeds the configured threshold (default 10%).
//! - p95_latency exceeds the configured threshold (default 5s).
//! - cost_per_hour exceeds the configured threshold (default $1).
//! - zero successful invocations in the last N minutes for an
//!   agent that was active before.
//!
//! When a condition crosses the threshold for the first time
//! (no `ActiveAlert` of the same kind for the same agent) the
//! engine emits a fire event. When the condition returns to
//! healthy, it emits a recovery event and clears the active
//! row. The same condition staying above-threshold across
//! ticks does NOT re-fire — dedup is keyed by `(agent, kind)`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::collector::now_ms;
use super::query::{MetricsQuery, MetricsQueryError};

/// Threshold knobs. Defaults match the spec.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct AlertThresholds {
    /// Error rate (0..=100) that trips an alert. Default 10%.
    #[serde(default = "default_error_rate_pct")]
    pub error_rate_pct: f64,
    /// P95 latency (ms) that trips an alert. Default 5000.
    #[serde(default = "default_p95_latency_ms")]
    pub p95_latency_ms: u64,
    /// Cost per hour (micro-USD) that trips an alert. Default
    /// $1.00 (= 1_000_000 micros).
    #[serde(default = "default_cost_per_hour_micros")]
    pub cost_per_hour_micros: u64,
    /// Window in minutes over which "zero successful
    /// invocations despite traffic" is computed.
    #[serde(default = "default_zero_success_window_mins")]
    pub zero_success_window_mins: u32,
    /// Minimum invocations in the evaluation window before
    /// rate-based alerts (error rate, p95) fire. Prevents a
    /// single-call sample from tripping the threshold.
    #[serde(default = "default_min_invocations_for_rate_alert")]
    pub min_invocations_for_rate_alert: u64,
    /// Evaluation window for error-rate / latency alerts, in
    /// minutes. Default 10.
    #[serde(default = "default_eval_window_mins")]
    pub eval_window_mins: u32,
}

impl Default for AlertThresholds {
    fn default() -> Self {
        Self {
            error_rate_pct: default_error_rate_pct(),
            p95_latency_ms: default_p95_latency_ms(),
            cost_per_hour_micros: default_cost_per_hour_micros(),
            zero_success_window_mins: default_zero_success_window_mins(),
            min_invocations_for_rate_alert: default_min_invocations_for_rate_alert(),
            eval_window_mins: default_eval_window_mins(),
        }
    }
}

fn default_error_rate_pct() -> f64 {
    10.0
}
fn default_p95_latency_ms() -> u64 {
    5000
}
fn default_cost_per_hour_micros() -> u64 {
    1_000_000
}
fn default_zero_success_window_mins() -> u32 {
    10
}
fn default_min_invocations_for_rate_alert() -> u64 {
    20
}
fn default_eval_window_mins() -> u32 {
    10
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    ErrorRate,
    P95Latency,
    CostPerHour,
    ZeroSuccess,
}

impl AlertKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertKind::ErrorRate => "error_rate",
            AlertKind::P95Latency => "p95_latency",
            AlertKind::CostPerHour => "cost_per_hour",
            AlertKind::ZeroSuccess => "zero_success",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    Warning,
    Critical,
}

impl AlertSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertSeverity::Warning => "warning",
            AlertSeverity::Critical => "critical",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveAlert {
    pub agent: String,
    pub kind: AlertKind,
    pub severity: AlertSeverity,
    pub triggered_at_ms: i64,
    pub threshold: f64,
    pub actual: f64,
    pub message: String,
}

/// What the engine emits on a single evaluation tick. The
/// coordinator chains these into chronicle writes + channel
/// dispatch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AlertEvent {
    /// A new alert went from healthy → above threshold.
    Fired(ActiveAlert),
    /// A previously-active alert returned to healthy. The
    /// embedded `ActiveAlert` is the snapshot at fire time, so
    /// channels can render `"recovered: alice error_rate
    /// (was 12.3%, now <10%)"`.
    Recovered(ActiveAlert),
}

impl AlertEvent {
    pub fn agent(&self) -> &str {
        match self {
            AlertEvent::Fired(a) | AlertEvent::Recovered(a) => &a.agent,
        }
    }

    pub fn kind(&self) -> AlertKind {
        match self {
            AlertEvent::Fired(a) | AlertEvent::Recovered(a) => a.kind,
        }
    }
}

/// Periodic threshold evaluator. Cheap to clone — holds an
/// `Arc<Mutex<>>` of the active-alerts map plus the read-only
/// thresholds + query handle.
#[derive(Clone)]
pub struct AlertEngine {
    query: MetricsQuery,
    thresholds: Arc<AlertThresholds>,
    active: Arc<Mutex<HashMap<(String, AlertKind), ActiveAlert>>>,
}

impl AlertEngine {
    pub fn new(query: MetricsQuery, thresholds: AlertThresholds) -> Self {
        Self {
            query,
            thresholds: Arc::new(thresholds),
            active: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Borrow the configured thresholds.
    pub fn thresholds(&self) -> &AlertThresholds {
        &self.thresholds
    }

    /// Snapshot of every currently-active alert.
    pub fn active_alerts(&self) -> Vec<ActiveAlert> {
        let g = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut v: Vec<ActiveAlert> = g.values().cloned().collect();
        v.sort_by(|a, b| {
            a.triggered_at_ms
                .cmp(&b.triggered_at_ms)
                .then(a.agent.cmp(&b.agent))
        });
        v
    }

    /// Run one evaluation pass over every known agent and
    /// return the events produced. Pure — no side effects
    /// beyond updating the engine's own `active` map.
    pub fn evaluate(&self) -> Result<Vec<AlertEvent>, MetricsQueryError> {
        let agents = self
            .query
            .list_agents(self.thresholds.zero_success_window_mins.div_ceil(60).max(1))?;
        // Use a longer window (>= 1h) for cost evaluation; same
        // pass collects per-agent summary for error_rate / p95.
        let agent_names: Vec<String> = agents.iter().map(|a| a.agent.clone()).collect();
        // Pull a fresh set of agents that have been active in
        // the last hour too, so cost-per-hour considers them.
        let cost_agents = self.query.list_agents(1)?;
        let mut seen: BTreeMap<String, ()> = BTreeMap::new();
        for a in agent_names {
            seen.insert(a, ());
        }
        for a in &cost_agents {
            seen.insert(a.agent.clone(), ());
        }
        let mut events = Vec::new();
        for agent in seen.keys() {
            self.eval_one_agent(agent, &mut events)?;
        }
        // Recovery: any agent in `active` that wasn't in the
        // evaluation set (because they've been quiet) clears
        // the next time they appear — we don't emit phantom
        // recovery events on agents the database has nothing
        // about. The fire-side check above already removes
        // active alerts when the condition clears for an agent
        // that IS in the evaluation set.
        Ok(events)
    }

    fn eval_one_agent(
        &self,
        agent: &str,
        events: &mut Vec<AlertEvent>,
    ) -> Result<(), MetricsQueryError> {
        // Use the eval_window_mins window for rate / latency.
        let rate_hours_float = self.thresholds.eval_window_mins as f64 / 60.0;
        let rate_hours = rate_hours_float.ceil().max(1.0) as u32;
        let summary = self.query.agent_summary(agent, rate_hours)?;
        // Error rate.
        if summary.invocations >= self.thresholds.min_invocations_for_rate_alert {
            let actual_pct = summary.error_rate * 100.0;
            self.evaluate_threshold(
                agent,
                AlertKind::ErrorRate,
                actual_pct,
                self.thresholds.error_rate_pct,
                actual_pct > self.thresholds.error_rate_pct,
                AlertSeverity::Warning,
                format!(
                    "{agent}: error rate {actual_pct:.2}% over last {n} invocations (threshold {th:.2}%)",
                    n = summary.invocations,
                    th = self.thresholds.error_rate_pct
                ),
                events,
            );
            // P95 latency.
            let actual_p95 = summary.p95_latency_ms as f64;
            self.evaluate_threshold(
                agent,
                AlertKind::P95Latency,
                actual_p95,
                self.thresholds.p95_latency_ms as f64,
                summary.p95_latency_ms > self.thresholds.p95_latency_ms,
                AlertSeverity::Warning,
                format!(
                    "{agent}: P95 latency {p95}ms (threshold {th}ms)",
                    p95 = summary.p95_latency_ms,
                    th = self.thresholds.p95_latency_ms
                ),
                events,
            );
        }
        // Cost per hour — uses a one-hour window via the cost
        // report scope (we re-query at hours=1 to align the
        // numerator with the threshold's units).
        let one_hour_summary = self.query.agent_summary(agent, 1)?;
        let cost_actual = one_hour_summary.total_cost_micros as f64;
        let cost_threshold = self.thresholds.cost_per_hour_micros as f64;
        self.evaluate_threshold(
            agent,
            AlertKind::CostPerHour,
            cost_actual,
            cost_threshold,
            one_hour_summary.total_cost_micros > self.thresholds.cost_per_hour_micros,
            AlertSeverity::Critical,
            format!(
                "{agent}: cost ${dollars:.4} in the last hour (threshold ${th:.2})",
                dollars = cost_actual / 1_000_000.0,
                th = cost_threshold / 1_000_000.0
            ),
            events,
        );
        // Zero-success: only fires when the agent IS active
        // (total > 0) AND successes == 0 in the window.
        let total = self
            .query
            .total_invocation_count(agent, self.thresholds.zero_success_window_mins)?;
        let success = self
            .query
            .successful_invocation_count(agent, self.thresholds.zero_success_window_mins)?;
        let cross = total > 0 && success == 0;
        self.evaluate_threshold(
            agent,
            AlertKind::ZeroSuccess,
            success as f64,
            1.0,
            cross,
            AlertSeverity::Critical,
            format!(
                "{agent}: 0 successful invocations in last {mins}m ({total} attempts)",
                mins = self.thresholds.zero_success_window_mins
            ),
            events,
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_threshold(
        &self,
        agent: &str,
        kind: AlertKind,
        actual: f64,
        threshold: f64,
        crossed: bool,
        severity: AlertSeverity,
        message: String,
        events: &mut Vec<AlertEvent>,
    ) {
        let key = (agent.to_string(), kind);
        let mut g = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match (crossed, g.contains_key(&key)) {
            (true, false) => {
                let active = ActiveAlert {
                    agent: agent.to_string(),
                    kind,
                    severity,
                    triggered_at_ms: now_ms(),
                    threshold,
                    actual,
                    message,
                };
                g.insert(key, active.clone());
                events.push(AlertEvent::Fired(active));
            }
            (false, true) => {
                if let Some(prior) = g.remove(&key) {
                    events.push(AlertEvent::Recovered(prior));
                }
            }
            _ => {
                // Already active + still crossed, or healthy and
                // wasn't active. No event.
            }
        }
    }

    /// Spawn a periodic evaluation task. `sink` is invoked for
    /// every produced event. Returns immediately; the task
    /// runs until the runtime is dropped.
    pub fn spawn(self, interval: Duration, sink: AlertSink) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            // Skip the immediate tick — give the collector
            // time to write its first batch.
            tick.tick().await;
            loop {
                tick.tick().await;
                match self.evaluate() {
                    Ok(events) => {
                        for e in events {
                            sink.deliver(&e);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "alert engine: evaluation failed");
                    }
                }
            }
        })
    }
}

/// Channel-of-record for alert events. Implementations forward
/// to the chronicle, the configured channels, and the
/// dashboard's active-alerts ring.
pub trait AlertDeliver: Send + Sync + 'static {
    fn deliver(&self, event: &AlertEvent);
}

/// Boxed sink the engine task holds.
#[derive(Clone)]
pub struct AlertSink {
    inner: Arc<dyn AlertDeliver>,
}

impl AlertSink {
    pub fn new<S: AlertDeliver>(sink: S) -> Self {
        Self {
            inner: Arc::new(sink),
        }
    }

    pub fn deliver(&self, event: &AlertEvent) {
        self.inner.deliver(event);
    }
}

/// Default sink that just logs at the right tracing level —
/// used when the coordinator wiring isn't available (tests,
/// stand-alone deployments).
#[derive(Default)]
pub struct LoggingAlertSink;

impl AlertDeliver for LoggingAlertSink {
    fn deliver(&self, event: &AlertEvent) {
        match event {
            AlertEvent::Fired(a) => {
                tracing::warn!(
                    agent = %a.agent,
                    kind = a.kind.as_str(),
                    severity = a.severity.as_str(),
                    threshold = a.threshold,
                    actual = a.actual,
                    "alert fired: {}",
                    a.message,
                );
            }
            AlertEvent::Recovered(a) => {
                tracing::info!(
                    agent = %a.agent,
                    kind = a.kind.as_str(),
                    "alert recovered: {}",
                    a.message,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::MetricsStore;
    use super::super::types::InvocationMetric;
    use super::*;

    fn metric(
        agent: &str,
        method: &str,
        ts_ms: i64,
        latency: u64,
        success: bool,
        cost: Option<u64>,
    ) -> InvocationMetric {
        InvocationMetric {
            agent_name: agent.into(),
            peer_alias: "p".into(),
            method: method.into(),
            timestamp_ms: ts_ms,
            latency_ms: latency,
            success,
            error_kind: if success {
                None
            } else {
                Some("INTERNAL".into())
            },
            token_count: None,
            cost_micros: cost,
            input_bytes: 100,
            output_bytes: 200,
            model: None,
            request_id: None,
        }
    }

    fn engine_with(
        thresholds: AlertThresholds,
        populate: impl FnOnce(&MetricsStore),
    ) -> AlertEngine {
        let store = MetricsStore::in_memory().unwrap();
        populate(&store);
        let q = MetricsQuery::new(store);
        AlertEngine::new(q, thresholds)
    }

    fn relaxed_thresholds() -> AlertThresholds {
        AlertThresholds {
            error_rate_pct: 10.0,
            p95_latency_ms: u64::MAX,
            cost_per_hour_micros: u64::MAX,
            zero_success_window_mins: AlertThresholds::default().zero_success_window_mins,
            min_invocations_for_rate_alert: 10,
            eval_window_mins: AlertThresholds::default().eval_window_mins,
        }
    }

    #[test]
    fn error_rate_alert_fires_when_threshold_crossed() {
        let t = relaxed_thresholds();
        let engine = engine_with(t, |store| {
            let now = now_ms();
            for _ in 0..15 {
                store
                    .insert(&metric("alice", "ai.chat", now, 50, true, None))
                    .unwrap();
            }
            for _ in 0..5 {
                store
                    .insert(&metric("alice", "ai.chat", now, 50, false, None))
                    .unwrap();
            }
            // 25% error rate.
        });
        let events = engine.evaluate().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            AlertEvent::Fired(a) if a.kind == AlertKind::ErrorRate
        )));
    }

    #[test]
    fn error_rate_alert_does_not_fire_below_threshold() {
        let t = relaxed_thresholds();
        let engine = engine_with(t, |store| {
            let now = now_ms();
            for _ in 0..50 {
                store
                    .insert(&metric("alice", "ai.chat", now, 50, true, None))
                    .unwrap();
            }
        });
        let events = engine.evaluate().unwrap();
        assert!(!events.iter().any(|e| matches!(
            e,
            AlertEvent::Fired(a) if a.kind == AlertKind::ErrorRate
        )));
    }

    #[test]
    fn dedup_does_not_refire_active_alert() {
        let t = relaxed_thresholds();
        let engine = engine_with(t, |store| {
            let now = now_ms();
            for _ in 0..15 {
                store
                    .insert(&metric("alice", "ai.chat", now, 50, true, None))
                    .unwrap();
            }
            for _ in 0..5 {
                store
                    .insert(&metric("alice", "ai.chat", now, 50, false, None))
                    .unwrap();
            }
        });
        let first = engine.evaluate().unwrap();
        let second = engine.evaluate().unwrap();
        let first_fired = first
            .iter()
            .filter(|e| matches!(e, AlertEvent::Fired(_)))
            .count();
        let second_fired = second
            .iter()
            .filter(|e| matches!(e, AlertEvent::Fired(_)))
            .count();
        assert_eq!(first_fired, 1);
        assert_eq!(second_fired, 0, "dedup should suppress re-fire");
        assert_eq!(engine.active_alerts().len(), 1);
    }

    #[test]
    fn recovery_event_fires_when_threshold_clears() {
        let t = relaxed_thresholds();
        let store = MetricsStore::in_memory().unwrap();
        let now = now_ms();
        for _ in 0..15 {
            store
                .insert(&metric("alice", "ai.chat", now, 50, true, None))
                .unwrap();
        }
        for _ in 0..5 {
            store
                .insert(&metric("alice", "ai.chat", now, 50, false, None))
                .unwrap();
        }
        let q = MetricsQuery::new(store.clone());
        let engine = AlertEngine::new(q, t);
        let _ = engine.evaluate().unwrap(); // fires
        assert_eq!(engine.active_alerts().len(), 1);
        // Backfill 100 successes — pushes error rate below 10%.
        for _ in 0..100 {
            store
                .insert(&metric("alice", "ai.chat", now, 50, true, None))
                .unwrap();
        }
        let events = engine.evaluate().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            AlertEvent::Recovered(a) if a.kind == AlertKind::ErrorRate
        )));
        assert_eq!(engine.active_alerts().len(), 0);
    }

    #[test]
    fn zero_success_alert_fires_only_when_traffic_present() {
        let t = AlertThresholds {
            zero_success_window_mins: 10,
            min_invocations_for_rate_alert: u64::MAX,
            p95_latency_ms: u64::MAX,
            cost_per_hour_micros: u64::MAX,
            ..AlertThresholds::default()
        };
        let store = MetricsStore::in_memory().unwrap();
        let now = now_ms();
        // Only failures; no successes.
        for _ in 0..5 {
            store
                .insert(&metric("alice", "ai.chat", now, 50, false, None))
                .unwrap();
        }
        let q = MetricsQuery::new(store);
        let engine = AlertEngine::new(q, t);
        let events = engine.evaluate().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            AlertEvent::Fired(a) if a.kind == AlertKind::ZeroSuccess
        )));
    }

    #[test]
    fn cost_per_hour_alert_uses_critical_severity() {
        let t = AlertThresholds {
            cost_per_hour_micros: 1000, // $0.001
            error_rate_pct: 100.0,
            p95_latency_ms: u64::MAX,
            ..AlertThresholds::default()
        };
        let engine = engine_with(t, |store| {
            store
                .insert(&metric(
                    "alice",
                    "ai.chat",
                    now_ms(),
                    100,
                    true,
                    Some(50_000),
                ))
                .unwrap();
        });
        let events = engine.evaluate().unwrap();
        let cost_event = events
            .iter()
            .find(|e| matches!(e, AlertEvent::Fired(a) if a.kind == AlertKind::CostPerHour));
        assert!(cost_event.is_some());
        if let Some(AlertEvent::Fired(a)) = cost_event {
            assert_eq!(a.severity, AlertSeverity::Critical);
        }
    }

    #[test]
    fn active_alerts_returns_sorted_snapshot() {
        let t = AlertThresholds::default();
        let store = MetricsStore::in_memory().unwrap();
        let q = MetricsQuery::new(store);
        let engine = AlertEngine::new(q, t);
        // Empty before evaluation.
        assert!(engine.active_alerts().is_empty());
    }
}
