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

    // ── GAP 22 Feature 2: provider-cost-spike + ask-human-rate drift ──
    /// GAP 22 Feature 2: multiplier above which a recent
    /// per-model hourly cost rate trips the spike alert. The
    /// recent rate is `model_cost_recent / hours_recent`; the
    /// baseline rate is `model_cost_baseline / hours_baseline`.
    /// Fires when `recent_rate > baseline_rate * factor`.
    /// Default 3.0 (= 300% of the rolling baseline).
    #[serde(default = "default_provider_cost_spike_factor")]
    pub provider_cost_spike_factor: f64,
    /// GAP 22 Feature 2: baseline window for the per-model
    /// cost rate, in hours. Default 24h.
    #[serde(default = "default_provider_cost_baseline_hours")]
    pub provider_cost_baseline_hours: u32,
    /// GAP 22 Feature 2: recent window for the per-model
    /// cost rate, in hours. Default 1h.
    #[serde(default = "default_provider_cost_recent_hours")]
    pub provider_cost_recent_hours: u32,
    /// GAP 22 Feature 2: noise floor for the per-model
    /// baseline. The spike alert only fires when the
    /// baseline window has at least this much accumulated
    /// cost (micro-USD). Default 10_000 = $0.01. Prevents an
    /// otherwise-quiet model from tripping on a single 3¢
    /// call.
    #[serde(default = "default_provider_cost_min_baseline_micros")]
    pub provider_cost_min_baseline_micros: u64,

    /// GAP 22 Feature 2: multiplier above which a recent
    /// per-agent ask-human rate trips the drift alert. The
    /// rate is `approval_required_count / total_count`
    /// over the configured window. Fires when
    /// `recent_rate > max(baseline_rate * factor, floor)`.
    /// Default 3.0.
    #[serde(default = "default_ask_human_drift_factor")]
    pub ask_human_drift_factor: f64,
    /// GAP 22 Feature 2: baseline window for the per-agent
    /// ask-human rate, in hours. Default 24h.
    #[serde(default = "default_ask_human_baseline_hours")]
    pub ask_human_baseline_hours: u32,
    /// GAP 22 Feature 2: recent window for the per-agent
    /// ask-human rate, in hours. Default 1h.
    #[serde(default = "default_ask_human_recent_hours")]
    pub ask_human_recent_hours: u32,
    /// GAP 22 Feature 2: noise-floor minimum-attempts in the
    /// recent window before the drift detector fires. Default
    /// 10. A single APPROVAL_REQUIRED in an otherwise-quiet
    /// window should not trip the alert.
    #[serde(default = "default_ask_human_min_attempts")]
    pub ask_human_min_attempts: u64,
    /// GAP 22 Feature 2: absolute-rate floor (0..=1.0) the
    /// recent rate must also exceed before the drift alert
    /// fires. Prevents 1/10 → 5/10 from tripping when the
    /// absolute rate is still operationally fine (= 0.05).
    /// Default 0.05 (5%).
    #[serde(default = "default_ask_human_min_recent_rate")]
    pub ask_human_min_recent_rate: f64,
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
            provider_cost_spike_factor: default_provider_cost_spike_factor(),
            provider_cost_baseline_hours: default_provider_cost_baseline_hours(),
            provider_cost_recent_hours: default_provider_cost_recent_hours(),
            provider_cost_min_baseline_micros: default_provider_cost_min_baseline_micros(),
            ask_human_drift_factor: default_ask_human_drift_factor(),
            ask_human_baseline_hours: default_ask_human_baseline_hours(),
            ask_human_recent_hours: default_ask_human_recent_hours(),
            ask_human_min_attempts: default_ask_human_min_attempts(),
            ask_human_min_recent_rate: default_ask_human_min_recent_rate(),
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
fn default_provider_cost_spike_factor() -> f64 {
    3.0
}
fn default_provider_cost_baseline_hours() -> u32 {
    24
}
fn default_provider_cost_recent_hours() -> u32 {
    1
}
fn default_provider_cost_min_baseline_micros() -> u64 {
    10_000
}
fn default_ask_human_drift_factor() -> f64 {
    3.0
}
fn default_ask_human_baseline_hours() -> u32 {
    24
}
fn default_ask_human_recent_hours() -> u32 {
    1
}
fn default_ask_human_min_attempts() -> u64 {
    10
}
fn default_ask_human_min_recent_rate() -> f64 {
    0.05
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    ErrorRate,
    P95Latency,
    CostPerHour,
    ZeroSuccess,
    /// RELIX-7.19 GAP 2: emitted by the dispatch bridge when
    /// the `ConfidenceScorer` returns a verdict below a
    /// configured `Alert` fallback threshold. Unlike the
    /// poll-driven kinds above, this kind is event-driven —
    /// the bridge calls
    /// [`AlertEngine::evaluate_low_confidence`] directly. The
    /// dedup key includes the called method so a chatty
    /// `tool.*` and a chatty `ai.chat` don't suppress each
    /// other.
    LowConfidence,
    /// RELIX-7.28 Part 1: emitted by the dispatch bridge's
    /// `BudgetEnforcer` when an agent or the deployment crosses
    /// a configured spend cap. Dedup is keyed by
    /// `(agent, "budget:<scope>:<window>", BudgetExceeded)` so
    /// daily / hourly trips at the same agent stay distinct,
    /// and agent-level breaches don't suppress the deployment
    /// roll-up.
    BudgetExceeded,
    /// GAP 22 Feature 2: emitted by the periodic evaluator
    /// when a model's recent (1h) cost rate exceeds its
    /// rolling (24h) baseline by a configurable factor (default
    /// 3×). Dedup is keyed per-model — the `agent` field
    /// carries `model:<model_id>` so different models alert
    /// independently and existing per-agent UI sort orders
    /// keep working.
    ProviderCostSpike,
    /// GAP 22 Feature 2: emitted by the periodic evaluator
    /// when an agent's recent (1h) `APPROVAL_REQUIRED` rate
    /// exceeds the agent's rolling (24h) baseline rate by a
    /// configurable factor. Dedup is keyed per-agent.
    AskHumanRateDrift,
    /// PART 3 + PART 4: generic absolute spend / cost guard
    /// trip. Emitted by the self-consistency cost guard when
    /// the rolling trigger-rate or hourly SC spend crosses its
    /// config limit, and by the absolute spend caps under
    /// `[metrics.cost_alerts]` when a request, hour, or day
    /// exceeds its hard ceiling — independent of any
    /// statistical baseline. The `message` field carries the
    /// cause string (e.g. `self_consistency_trigger_rate_exceeded`,
    /// `absolute_hourly_cap_exceeded`).
    CostAlert,
}

impl AlertKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertKind::ErrorRate => "error_rate",
            AlertKind::P95Latency => "p95_latency",
            AlertKind::CostPerHour => "cost_per_hour",
            AlertKind::ZeroSuccess => "zero_success",
            AlertKind::LowConfidence => "low_confidence",
            AlertKind::BudgetExceeded => "budget_exceeded",
            AlertKind::ProviderCostSpike => "provider_cost_spike",
            AlertKind::AskHumanRateDrift => "ask_human_rate_drift",
            AlertKind::CostAlert => "cost_alert",
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
    /// RELIX-7.19 GAP 2: capability method this alert was
    /// raised against. `None` for poll-driven kinds
    /// (`ErrorRate`, `P95Latency`, `CostPerHour`,
    /// `ZeroSuccess`) that operate over an entire agent's
    /// metric stream. `Some(method)` for `LowConfidence` so
    /// the dedup key + chronicle row + channel formatting
    /// all carry which capability tripped the threshold.
    /// Additive — older serialised rows decode as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
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
///
/// RELIX-7.19 GAP 2: the dedup key carries an optional
/// `method` so `LowConfidence` alerts can dedup per
/// `(agent, method, kind)` while the poll-driven kinds keep
/// dedup per `(agent, kind)` (method = `None`).
#[derive(Clone)]
pub struct AlertEngine {
    query: MetricsQuery,
    thresholds: Arc<AlertThresholds>,
    active: Arc<Mutex<HashMap<DedupKey, ActiveAlert>>>,
}

/// Per-alert dedup key. Three components keep the per-kind
/// semantics straight: `agent`, `method` (Some for
/// `LowConfidence`, None for poll-driven kinds), and `kind`.
pub(crate) type DedupKey = (String, Option<String>, AlertKind);

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
            // GAP 22 Feature 2: ask-human-rate drift runs per
            // agent — it reads the agent's recent vs baseline
            // APPROVAL_REQUIRED ratio from the metrics store.
            self.eval_ask_human_drift(agent, &mut events)?;
        }
        // GAP 22 Feature 2: provider-cost-spike runs per
        // model. Re-pull the model list against the baseline
        // window so a recently-quiet model that drove cost
        // earlier in the day still gets its baseline computed.
        let baseline_h = self.thresholds.provider_cost_baseline_hours.max(1);
        let models = self.query.list_models(baseline_h)?;
        for model in &models {
            self.eval_provider_cost_spike(model, &mut events)?;
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

    /// GAP 22 Feature 2: per-agent ask-human-rate drift
    /// evaluator. Pure read; the only side effect is the
    /// engine's own `active` map updating via
    /// `evaluate_threshold_keyed`.
    fn eval_ask_human_drift(
        &self,
        agent: &str,
        events: &mut Vec<AlertEvent>,
    ) -> Result<(), MetricsQueryError> {
        let baseline_h = self.thresholds.ask_human_baseline_hours.max(1);
        let recent_h = self.thresholds.ask_human_recent_hours.max(1);
        let (baseline_approvals, baseline_total) = self.query.ask_human_rate(agent, baseline_h)?;
        let (recent_approvals, recent_total) = self.query.ask_human_rate(agent, recent_h)?;

        // Noise floor: drop the alert when the recent window
        // doesn't have enough traffic to be confident in the
        // ratio. The min-attempts check is on the recent window
        // because that's the numerator being compared.
        if recent_total < self.thresholds.ask_human_min_attempts {
            self.evaluate_threshold_keyed(
                agent,
                None,
                AlertKind::AskHumanRateDrift,
                0.0,
                self.thresholds.ask_human_drift_factor,
                false,
                AlertSeverity::Warning,
                String::new(),
                events,
            );
            return Ok(());
        }

        let recent_rate = recent_approvals as f64 / recent_total.max(1) as f64;
        let baseline_rate = if baseline_total == 0 {
            0.0
        } else {
            baseline_approvals as f64 / baseline_total as f64
        };
        // Effective threshold is the higher of "factor × baseline"
        // and the absolute-rate floor. The floor stops a tiny
        // baseline (e.g. 0/100 → recent 1/10) from tripping.
        let drift_threshold = (baseline_rate * self.thresholds.ask_human_drift_factor)
            .max(self.thresholds.ask_human_min_recent_rate);
        let crossed = recent_rate > drift_threshold && recent_rate > 0.0;
        let message = format!(
            "{agent}: ask-human rate {pct:.2}% over last {h}h ({n} APPROVAL_REQUIRED / {t} calls; \
             baseline {bpct:.2}% over {bh}h; threshold {tpct:.2}%)",
            pct = recent_rate * 100.0,
            h = recent_h,
            n = recent_approvals,
            t = recent_total,
            bpct = baseline_rate * 100.0,
            bh = baseline_h,
            tpct = drift_threshold * 100.0,
        );
        self.evaluate_threshold_keyed(
            agent,
            None,
            AlertKind::AskHumanRateDrift,
            recent_rate,
            drift_threshold,
            crossed,
            AlertSeverity::Warning,
            message,
            events,
        );
        Ok(())
    }

    /// GAP 22 Feature 2: per-model cost-spike evaluator. Dedup
    /// is keyed on a synthetic agent name `model:<id>` so
    /// existing dashboards that sort by agent get a clean
    /// `model:gpt-4o-mini` row, and operators reading the
    /// chronicle can immediately filter to provider-cost rows.
    fn eval_provider_cost_spike(
        &self,
        model: &str,
        events: &mut Vec<AlertEvent>,
    ) -> Result<(), MetricsQueryError> {
        let baseline_h = self.thresholds.provider_cost_baseline_hours.max(1);
        let recent_h = self.thresholds.provider_cost_recent_hours.max(1);
        let (baseline_cost, _) = self.query.model_cost_summary(model, baseline_h)?;
        let (recent_cost, _) = self.query.model_cost_summary(model, recent_h)?;

        // Noise floor: drop the alert when the baseline window
        // hasn't accumulated enough cost to make the spike
        // ratio meaningful.
        if baseline_cost < self.thresholds.provider_cost_min_baseline_micros {
            self.evaluate_threshold_keyed(
                &format!("model:{model}"),
                None,
                AlertKind::ProviderCostSpike,
                0.0,
                self.thresholds.provider_cost_spike_factor,
                false,
                AlertSeverity::Warning,
                String::new(),
                events,
            );
            return Ok(());
        }

        let baseline_rate_per_hour = baseline_cost as f64 / baseline_h as f64;
        let recent_rate_per_hour = recent_cost as f64 / recent_h as f64;
        let threshold_rate_per_hour =
            baseline_rate_per_hour * self.thresholds.provider_cost_spike_factor;
        let crossed = recent_rate_per_hour > threshold_rate_per_hour;
        let message = format!(
            "model:{model}: ${dollars:.4}/h over last {h}h (baseline ${bdollars:.4}/h over {bh}h; \
             spike factor {factor:.1}×, threshold ${tdollars:.4}/h)",
            dollars = recent_rate_per_hour / 1_000_000.0,
            h = recent_h,
            bdollars = baseline_rate_per_hour / 1_000_000.0,
            bh = baseline_h,
            factor = self.thresholds.provider_cost_spike_factor,
            tdollars = threshold_rate_per_hour / 1_000_000.0,
        );
        self.evaluate_threshold_keyed(
            &format!("model:{model}"),
            None,
            AlertKind::ProviderCostSpike,
            recent_rate_per_hour,
            threshold_rate_per_hour,
            crossed,
            AlertSeverity::Warning,
            message,
            events,
        );
        Ok(())
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
        self.evaluate_threshold_keyed(
            agent, None, kind, actual, threshold, crossed, severity, message, events,
        );
    }

    /// RELIX-7.19 GAP 2: key-extended variant. `method` is
    /// `None` for poll-driven kinds and `Some(method)` for
    /// `LowConfidence` so the per-method dedup is exact.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_threshold_keyed(
        &self,
        agent: &str,
        method: Option<&str>,
        kind: AlertKind,
        actual: f64,
        threshold: f64,
        crossed: bool,
        severity: AlertSeverity,
        message: String,
        events: &mut Vec<AlertEvent>,
    ) {
        let key: DedupKey = (agent.to_string(), method.map(|s| s.to_string()), kind);
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
                    method: method.map(|s| s.to_string()),
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

    /// RELIX-7.19 GAP 2: event-driven low-confidence
    /// evaluator the dispatch bridge calls every time its
    /// `Alert` fallback action fires. Behaves like
    /// `evaluate_threshold` but: (a) keyed on
    /// `(agent, method, LowConfidence)` so per-cap brittle
    /// providers don't suppress each other, (b) severity is
    /// derived from the score-vs-threshold split — at-or-
    /// below `critical_threshold` raises `Critical`, otherwise
    /// `Warning`, (c) returns the event list directly so the
    /// caller can deliver synchronously. Dedup semantics
    /// match the polled kinds: a `Fired` event is emitted on
    /// the crossing edge, a `Recovered` event is emitted when
    /// the score climbs back above `low_threshold`, and
    /// neither is emitted for already-active or already-clear
    /// states.
    pub fn evaluate_low_confidence(
        &self,
        agent: &str,
        method: &str,
        score: f32,
        low_threshold: f32,
        critical_threshold: f32,
        message: impl Into<String>,
    ) -> Vec<AlertEvent> {
        let mut events = Vec::new();
        let crossed = score <= low_threshold;
        let severity = if score <= critical_threshold {
            AlertSeverity::Critical
        } else {
            AlertSeverity::Warning
        };
        let threshold = if score <= critical_threshold {
            critical_threshold as f64
        } else {
            low_threshold as f64
        };
        self.evaluate_threshold_keyed(
            agent,
            Some(method),
            AlertKind::LowConfidence,
            score as f64,
            threshold,
            crossed,
            severity,
            message.into(),
            &mut events,
        );
        events
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
            tenant_id: "default".into(),
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
            confidence_score: None,
            routing_tier: None,
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
            min_invocations_for_rate_alert: 10,
            // GAP 22 Feature 2: push the spike + drift
            // thresholds out of the way for existing tests
            // that don't touch the cost or APPROVAL_REQUIRED
            // signal.
            provider_cost_spike_factor: f64::MAX,
            provider_cost_min_baseline_micros: u64::MAX,
            ask_human_drift_factor: f64::MAX,
            ask_human_min_attempts: u64::MAX,
            ask_human_min_recent_rate: 1.0,
            ..AlertThresholds::default()
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

    // ── RELIX-7.19 GAP 2: LowConfidence dedup tests ─────────

    fn empty_engine() -> AlertEngine {
        let store = MetricsStore::in_memory().unwrap();
        let q = MetricsQuery::new(store);
        AlertEngine::new(q, AlertThresholds::default())
    }

    #[test]
    fn low_confidence_alert_fires_when_score_below_low_threshold() {
        let engine = empty_engine();
        let events =
            engine.evaluate_low_confidence("alice", "ai.chat", 0.40, 0.50, 0.30, "low confidence");
        assert_eq!(events.len(), 1, "expected one Fired event");
        match &events[0] {
            AlertEvent::Fired(a) => {
                assert_eq!(a.kind, AlertKind::LowConfidence);
                assert_eq!(a.agent, "alice");
                assert_eq!(a.method.as_deref(), Some("ai.chat"));
                assert_eq!(a.severity, AlertSeverity::Warning);
                assert!((a.actual - 0.40).abs() < 1e-3);
            }
            o => panic!("expected Fired, got {o:?}"),
        }
    }

    #[test]
    fn low_confidence_critical_severity_when_score_below_critical_threshold() {
        let engine = empty_engine();
        let events =
            engine.evaluate_low_confidence("alice", "ai.chat", 0.20, 0.50, 0.30, "very low");
        match &events[0] {
            AlertEvent::Fired(a) => assert_eq!(a.severity, AlertSeverity::Critical),
            o => panic!("expected Fired, got {o:?}"),
        }
    }

    #[test]
    fn low_confidence_does_not_re_fire_while_active() {
        let engine = empty_engine();
        let first = engine.evaluate_low_confidence("alice", "ai.chat", 0.40, 0.50, 0.30, "msg");
        let second = engine.evaluate_low_confidence("alice", "ai.chat", 0.35, 0.50, 0.30, "msg");
        assert_eq!(first.len(), 1, "first call fires");
        assert!(second.is_empty(), "second call dedups: {second:?}");
    }

    #[test]
    fn low_confidence_clears_when_score_recovers_above_low_threshold() {
        let engine = empty_engine();
        let _ = engine.evaluate_low_confidence("alice", "ai.chat", 0.40, 0.50, 0.30, "msg");
        let recovery = engine.evaluate_low_confidence("alice", "ai.chat", 0.85, 0.50, 0.30, "msg");
        assert_eq!(recovery.len(), 1, "recovery event expected");
        match &recovery[0] {
            AlertEvent::Recovered(a) => {
                assert_eq!(a.kind, AlertKind::LowConfidence);
                assert_eq!(a.method.as_deref(), Some("ai.chat"));
            }
            o => panic!("expected Recovered, got {o:?}"),
        }
    }

    #[test]
    fn low_confidence_dedup_is_per_agent_method_not_per_agent() {
        let engine = empty_engine();
        let a = engine.evaluate_low_confidence("alice", "ai.chat", 0.4, 0.5, 0.3, "m1");
        let b = engine.evaluate_low_confidence("alice", "tool.web", 0.4, 0.5, 0.3, "m2");
        let c = engine.evaluate_low_confidence("alice", "ai.chat", 0.4, 0.5, 0.3, "m1-dup");
        assert_eq!(a.len(), 1, "alice/ai.chat fires");
        assert_eq!(b.len(), 1, "alice/tool.web fires (different method)");
        assert!(c.is_empty(), "alice/ai.chat dedups: {c:?}");
    }

    // ---- GAP 22 Feature 2: provider-cost-spike + ask-human-rate drift ----

    fn metric_full(
        agent: &str,
        method: &str,
        ts_ms: i64,
        model: Option<&str>,
        cost_micros: Option<u64>,
        success: bool,
        error_kind: Option<&str>,
    ) -> InvocationMetric {
        InvocationMetric {
            agent_name: agent.into(),
            tenant_id: "default".into(),
            peer_alias: "p".into(),
            method: method.into(),
            timestamp_ms: ts_ms,
            latency_ms: 10,
            success,
            error_kind: error_kind.map(|s| s.to_string()),
            token_count: None,
            cost_micros,
            input_bytes: 0,
            output_bytes: 0,
            model: model.map(|s| s.to_string()),
            confidence_score: None,
            routing_tier: None,
            request_id: None,
        }
    }

    fn spike_only_thresholds() -> AlertThresholds {
        // Disable every other kind so the assertion windows
        // stay clean.
        AlertThresholds {
            error_rate_pct: 100.1,
            p95_latency_ms: u64::MAX,
            cost_per_hour_micros: u64::MAX,
            min_invocations_for_rate_alert: u64::MAX,
            ask_human_drift_factor: f64::MAX,
            ask_human_min_attempts: u64::MAX,
            ask_human_min_recent_rate: 1.0,
            ..AlertThresholds::default()
        }
    }

    fn drift_only_thresholds() -> AlertThresholds {
        AlertThresholds {
            error_rate_pct: 100.1,
            p95_latency_ms: u64::MAX,
            cost_per_hour_micros: u64::MAX,
            min_invocations_for_rate_alert: u64::MAX,
            provider_cost_spike_factor: f64::MAX,
            provider_cost_min_baseline_micros: u64::MAX,
            ..AlertThresholds::default()
        }
    }

    #[test]
    fn model_cost_summary_aggregates_rows_within_the_window() {
        let store = MetricsStore::in_memory().unwrap();
        let now = now_ms();
        store
            .insert(&metric_full(
                "a",
                "ai.chat",
                now,
                Some("gpt-4o-mini"),
                Some(1_000_000),
                true,
                None,
            ))
            .unwrap();
        store
            .insert(&metric_full(
                "a",
                "ai.chat",
                now,
                Some("gpt-4o-mini"),
                Some(500_000),
                true,
                None,
            ))
            .unwrap();
        // Different model, same window — must not be included.
        store
            .insert(&metric_full(
                "a",
                "ai.chat",
                now,
                Some("claude-3-5-sonnet"),
                Some(2_000_000),
                true,
                None,
            ))
            .unwrap();
        let q = MetricsQuery::new(store);
        let (cost, n) = q.model_cost_summary("gpt-4o-mini", 1).unwrap();
        assert_eq!(cost, 1_500_000);
        assert_eq!(n, 2);
    }

    #[test]
    fn list_models_returns_distinct_non_empty_values() {
        let store = MetricsStore::in_memory().unwrap();
        let now = now_ms();
        store
            .insert(&metric_full(
                "a",
                "ai.chat",
                now,
                Some("m1"),
                Some(100),
                true,
                None,
            ))
            .unwrap();
        store
            .insert(&metric_full(
                "a",
                "ai.chat",
                now,
                Some("m1"),
                Some(100),
                true,
                None,
            ))
            .unwrap();
        store
            .insert(&metric_full(
                "a",
                "ai.chat",
                now,
                Some("m2"),
                Some(100),
                true,
                None,
            ))
            .unwrap();
        // NULL model row must be excluded.
        store
            .insert(&metric_full(
                "a",
                "ai.chat",
                now,
                None,
                Some(100),
                true,
                None,
            ))
            .unwrap();
        let q = MetricsQuery::new(store);
        let mut models = q.list_models(24).unwrap();
        models.sort();
        assert_eq!(models, vec!["m1".to_string(), "m2".to_string()]);
    }

    #[test]
    fn ask_human_rate_counts_approval_required_rows_per_agent() {
        let store = MetricsStore::in_memory().unwrap();
        let now = now_ms();
        for _ in 0..3 {
            store
                .insert(&metric_full(
                    "alice",
                    "ai.chat",
                    now,
                    None,
                    None,
                    false,
                    Some("APPROVAL_REQUIRED"),
                ))
                .unwrap();
        }
        for _ in 0..7 {
            store
                .insert(&metric_full(
                    "alice", "ai.chat", now, None, None, true, None,
                ))
                .unwrap();
        }
        // bob should not pollute alice's count.
        store
            .insert(&metric_full(
                "bob",
                "ai.chat",
                now,
                None,
                None,
                false,
                Some("APPROVAL_REQUIRED"),
            ))
            .unwrap();
        let q = MetricsQuery::new(store);
        let (approvals, total) = q.ask_human_rate("alice", 24).unwrap();
        assert_eq!(approvals, 3);
        assert_eq!(total, 10);
    }

    #[test]
    fn provider_cost_spike_fires_when_recent_rate_exceeds_baseline_by_factor() {
        let mut t = spike_only_thresholds();
        // Make the math obvious.
        t.provider_cost_spike_factor = 3.0;
        t.provider_cost_baseline_hours = 24;
        t.provider_cost_recent_hours = 1;
        t.provider_cost_min_baseline_micros = 1_000; // 0.001 USD
        let engine = engine_with(t, |store| {
            let now = now_ms();
            let one_hour = 60 * 60 * 1000_i64;
            // Baseline: $0.24 spread across 24h (1c/hour rate
            // averaged). Plant one row per hour over the last
            // 24h. Each row is $0.01.
            for h in 1..=24 {
                store
                    .insert(&metric_full(
                        "a",
                        "ai.chat",
                        now - (h * one_hour),
                        Some("gpt-4o-mini"),
                        Some(10_000),
                        true,
                        None,
                    ))
                    .unwrap();
            }
            // Recent (last hour): $0.50 in a single call — 50×
            // the baseline rate. Far above the 3× threshold.
            store
                .insert(&metric_full(
                    "a",
                    "ai.chat",
                    now,
                    Some("gpt-4o-mini"),
                    Some(500_000),
                    true,
                    None,
                ))
                .unwrap();
        });
        let events = engine.evaluate().unwrap();
        let fired: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, AlertEvent::Fired(a) if a.kind == AlertKind::ProviderCostSpike))
            .collect();
        assert_eq!(fired.len(), 1, "spike must fire: {events:?}");
        if let AlertEvent::Fired(a) = fired[0] {
            assert_eq!(a.agent, "model:gpt-4o-mini");
            assert!(a.message.contains("baseline"));
        }
    }

    #[test]
    fn provider_cost_spike_respects_min_baseline_noise_floor() {
        let mut t = spike_only_thresholds();
        t.provider_cost_spike_factor = 3.0;
        t.provider_cost_baseline_hours = 24;
        t.provider_cost_recent_hours = 1;
        // Force the noise floor up so the modest baseline
        // doesn't qualify.
        t.provider_cost_min_baseline_micros = 1_000_000_000; // $1000
        let engine = engine_with(t, |store| {
            let now = now_ms();
            // Baseline: 1¢. Recent: 100×. Would fire on factor
            // alone, but baseline noise floor blocks it.
            store
                .insert(&metric_full(
                    "a",
                    "ai.chat",
                    now - 60_000,
                    Some("gpt-4o-mini"),
                    Some(10_000),
                    true,
                    None,
                ))
                .unwrap();
            store
                .insert(&metric_full(
                    "a",
                    "ai.chat",
                    now,
                    Some("gpt-4o-mini"),
                    Some(1_000_000),
                    true,
                    None,
                ))
                .unwrap();
        });
        let events = engine.evaluate().unwrap();
        assert!(
            !events.iter().any(|e| matches!(
                e,
                AlertEvent::Fired(a) if a.kind == AlertKind::ProviderCostSpike
            )),
            "noise floor must block: {events:?}"
        );
    }

    #[test]
    fn ask_human_drift_fires_when_recent_rate_exceeds_baseline_by_factor() {
        let mut t = drift_only_thresholds();
        t.ask_human_drift_factor = 3.0;
        t.ask_human_baseline_hours = 24;
        t.ask_human_recent_hours = 1;
        t.ask_human_min_attempts = 10;
        t.ask_human_min_recent_rate = 0.01;
        let engine = engine_with(t, |store| {
            let now = now_ms();
            let one_hour = 60 * 60 * 1000_i64;
            // Baseline: 1/200 = 0.5% over 24h (well-mixed).
            // Plant rows across the 24h window.
            for h in 1..=24 {
                for _ in 0..8 {
                    store
                        .insert(&metric_full(
                            "alice",
                            "ai.chat",
                            now - (h * one_hour),
                            None,
                            None,
                            true,
                            None,
                        ))
                        .unwrap();
                }
            }
            // One APPROVAL_REQUIRED somewhere in the baseline.
            store
                .insert(&metric_full(
                    "alice",
                    "ai.chat",
                    now - one_hour * 8,
                    None,
                    None,
                    false,
                    Some("APPROVAL_REQUIRED"),
                ))
                .unwrap();
            // Recent hour: 5/10 = 50% — way over the 3× × 0.5%
            // = 1.5% threshold.
            for _ in 0..5 {
                store
                    .insert(&metric_full(
                        "alice",
                        "ai.chat",
                        now,
                        None,
                        None,
                        false,
                        Some("APPROVAL_REQUIRED"),
                    ))
                    .unwrap();
            }
            for _ in 0..5 {
                store
                    .insert(&metric_full(
                        "alice", "ai.chat", now, None, None, true, None,
                    ))
                    .unwrap();
            }
        });
        let events = engine.evaluate().unwrap();
        let fired: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, AlertEvent::Fired(a) if a.kind == AlertKind::AskHumanRateDrift))
            .collect();
        assert_eq!(fired.len(), 1, "drift must fire: {events:?}");
        if let AlertEvent::Fired(a) = fired[0] {
            assert_eq!(a.agent, "alice");
            assert!(a.message.contains("ask-human rate"));
        }
    }

    #[test]
    fn ask_human_drift_does_not_fire_below_min_attempts_floor() {
        let mut t = drift_only_thresholds();
        t.ask_human_drift_factor = 3.0;
        t.ask_human_baseline_hours = 24;
        t.ask_human_recent_hours = 1;
        // Force min_attempts up so the small recent window
        // doesn't qualify.
        t.ask_human_min_attempts = 100;
        t.ask_human_min_recent_rate = 0.0;
        let engine = engine_with(t, |store| {
            let now = now_ms();
            // 5 of 5 recent calls are APPROVAL_REQUIRED. The
            // ratio is 100%, far above any factor — but the
            // sample size of 5 falls below the 100-attempt
            // floor.
            for _ in 0..5 {
                store
                    .insert(&metric_full(
                        "alice",
                        "ai.chat",
                        now,
                        None,
                        None,
                        false,
                        Some("APPROVAL_REQUIRED"),
                    ))
                    .unwrap();
            }
        });
        let events = engine.evaluate().unwrap();
        assert!(
            !events.iter().any(|e| matches!(
                e,
                AlertEvent::Fired(a) if a.kind == AlertKind::AskHumanRateDrift
            )),
            "min-attempts floor must block: {events:?}"
        );
    }

    #[test]
    fn ask_human_drift_respects_absolute_min_recent_rate_floor() {
        let mut t = drift_only_thresholds();
        t.ask_human_drift_factor = 3.0;
        t.ask_human_baseline_hours = 24;
        t.ask_human_recent_hours = 1;
        t.ask_human_min_attempts = 10;
        // Force the absolute-rate floor up so the modest
        // recent rate (10%) doesn't qualify.
        t.ask_human_min_recent_rate = 0.5; // 50%
        let engine = engine_with(t, |store| {
            let now = now_ms();
            // Recent: 1/10 = 10% APPROVAL_REQUIRED. Baseline
            // is zero (no earlier rows). Recent > 3 × 0 BUT
            // 10% < 50% absolute floor → no fire.
            for _ in 0..9 {
                store
                    .insert(&metric_full(
                        "alice", "ai.chat", now, None, None, true, None,
                    ))
                    .unwrap();
            }
            store
                .insert(&metric_full(
                    "alice",
                    "ai.chat",
                    now,
                    None,
                    None,
                    false,
                    Some("APPROVAL_REQUIRED"),
                ))
                .unwrap();
        });
        let events = engine.evaluate().unwrap();
        assert!(
            !events.iter().any(|e| matches!(
                e,
                AlertEvent::Fired(a) if a.kind == AlertKind::AskHumanRateDrift
            )),
            "absolute-rate floor must block: {events:?}"
        );
    }

    #[test]
    fn alert_kind_round_trips_new_variants_to_their_snake_case_strings() {
        assert_eq!(AlertKind::ProviderCostSpike.as_str(), "provider_cost_spike");
        assert_eq!(
            AlertKind::AskHumanRateDrift.as_str(),
            "ask_human_rate_drift"
        );
    }
}
