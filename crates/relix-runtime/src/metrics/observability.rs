//! RELIX-7.28 Part 2 — operator-facing observability dashboard surface.
//!
//! Three coordinator-side capabilities aggregate the existing metrics +
//! alert chronicle + budget enforcer state into a single operator surface:
//!
//! - `observability.active_alerts` — every currently-firing alert sorted
//!   by severity then recency.
//! - `observability.alert_history` — the last N chronicle rows, optionally
//!   filtered by `agent`.
//! - `observability.health_summary` — per-agent health scores (0–100)
//!   weighted across error rate, latency, confidence trend, and budget
//!   utilisation. Plus a deployment-wide roll-up.
//!
//! The capabilities never mutate state — they read from the alert engine
//! (dedup state), the alert chronicle (history), the metrics query engine
//! (per-agent summary), and the optional budget enforcer (utilisation).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use relix_core::types::{ErrorEnvelope, error_kinds};

use super::alert::{ActiveAlert, AlertEngine, AlertSeverity};
use super::alert_delivery::{AlertChronicle, AlertChronicleRow};
use super::budget::BudgetEnforcer;
use super::query::MetricsQuery;

/// One row in the deployment health summary. Cheap to serialise.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentHealthRow {
    pub agent: String,
    /// 0..=100. Composite score; never NaN.
    pub score: u32,
    /// `"green"` (80+) / `"yellow"` (50..80) / `"red"` (<50).
    pub status: String,
    pub error_rate_pct: f64,
    pub p95_latency_ms: u64,
    pub avg_confidence: Option<f64>,
    pub daily_budget_utilization_pct: Option<f64>,
    /// Number of currently-active alerts touching this agent.
    pub active_alerts: u64,
}

/// Deployment-wide roll-up.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DeploymentHealthSummary {
    pub total_cost_usd: f64,
    pub total_invocations: u64,
    pub overall_error_rate_pct: f64,
    pub active_alert_count: u64,
    /// Mean of the per-agent score; `0` when there are no agents.
    pub avg_health_score: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct HealthSummary {
    pub agents: Vec<AgentHealthRow>,
    pub deployment: DeploymentHealthSummary,
    pub window_hours: u32,
}

#[derive(Debug, Default, Deserialize)]
struct HistoryArgs {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    agent: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct HealthArgs {
    #[serde(default)]
    hours: Option<u32>,
}

/// Wire the three observability capabilities into the coordinator's
/// dispatch bridge. `query` + `chronicle` come from the metrics bundle;
/// `engine` from the alert engine; `enforcer` from the budget bundle
/// (None when budgets are dormant).
pub fn register(
    bridge: &mut DispatchBridge,
    query: MetricsQuery,
    chronicle: AlertChronicle,
    engine: Arc<AlertEngine>,
    enforcer: Option<Arc<BudgetEnforcer>>,
) {
    register_active_alerts(bridge, engine.clone());
    register_alert_history(bridge, chronicle);
    register_health_summary(bridge, query, engine, enforcer);
}

fn register_active_alerts(bridge: &mut DispatchBridge, engine: Arc<AlertEngine>) {
    bridge.register(
        "observability.active_alerts",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| {
            let engine = engine.clone();
            async move {
                let mut alerts = engine.active_alerts();
                sort_alerts_for_dashboard(&mut alerts);
                match serde_json::to_vec(&alerts) {
                    Ok(b) => HandlerOutcome::Ok(b),
                    Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::RESPONDER_INTERNAL,
                        cause: format!("observability.active_alerts encode: {e}"),
                        retry_hint: 0,
                        retry_after: None,
                    }),
                }
            }
        })),
    );
}

fn register_alert_history(bridge: &mut DispatchBridge, chronicle: AlertChronicle) {
    bridge.register(
        "observability.alert_history",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let chronicle = chronicle.clone();
            async move {
                let args: HistoryArgs = if ctx.args.is_empty() {
                    HistoryArgs::default()
                } else {
                    match serde_json::from_slice(&ctx.args) {
                        Ok(a) => a,
                        Err(e) => {
                            return HandlerOutcome::Err(ErrorEnvelope {
                                kind: error_kinds::INVALID_ARGS,
                                cause: format!("decode args: {e}"),
                                retry_hint: 0,
                                retry_after: None,
                            });
                        }
                    }
                };
                let limit = args.limit.unwrap_or(50).clamp(1, 1000);
                let mut rows = match chronicle.recent(limit) {
                    Ok(rows) => rows,
                    Err(e) => {
                        return HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::RESPONDER_INTERNAL,
                            cause: format!("chronicle read: {e}"),
                            retry_hint: 0,
                            retry_after: None,
                        });
                    }
                };
                if let Some(agent) = args.agent.as_deref() {
                    rows.retain(|r: &AlertChronicleRow| r.agent == agent);
                }
                match serde_json::to_vec(&rows) {
                    Ok(b) => HandlerOutcome::Ok(b),
                    Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::RESPONDER_INTERNAL,
                        cause: format!("observability.alert_history encode: {e}"),
                        retry_hint: 0,
                        retry_after: None,
                    }),
                }
            }
        })),
    );
}

fn register_health_summary(
    bridge: &mut DispatchBridge,
    query: MetricsQuery,
    engine: Arc<AlertEngine>,
    enforcer: Option<Arc<BudgetEnforcer>>,
) {
    bridge.register(
        "observability.health_summary",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let query = query.clone();
            let engine = engine.clone();
            let enforcer = enforcer.clone();
            async move {
                let args: HealthArgs = if ctx.args.is_empty() {
                    HealthArgs::default()
                } else {
                    match serde_json::from_slice(&ctx.args) {
                        Ok(a) => a,
                        Err(e) => {
                            return HandlerOutcome::Err(ErrorEnvelope {
                                kind: error_kinds::INVALID_ARGS,
                                cause: format!("decode args: {e}"),
                                retry_hint: 0,
                                retry_after: None,
                            });
                        }
                    }
                };
                let hours = args.hours.unwrap_or(24).clamp(1, 24 * 90);
                let summary =
                    compute_health_summary(&query, &engine, enforcer.as_ref(), hours).await;
                let summary = match summary {
                    Ok(s) => s,
                    Err(e) => {
                        return HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::RESPONDER_INTERNAL,
                            cause: format!("health summary: {e}"),
                            retry_hint: 0,
                            retry_after: None,
                        });
                    }
                };
                match serde_json::to_vec(&summary) {
                    Ok(b) => HandlerOutcome::Ok(b),
                    Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::RESPONDER_INTERNAL,
                        cause: format!("observability.health_summary encode: {e}"),
                        retry_hint: 0,
                        retry_after: None,
                    }),
                }
            }
        })),
    );
}

/// Sort active alerts so the dashboard renders the most important first:
/// critical-severity before warning, then newest within a severity bucket.
pub fn sort_alerts_for_dashboard(alerts: &mut [ActiveAlert]) {
    alerts.sort_by(|a, b| {
        severity_rank(b.severity)
            .cmp(&severity_rank(a.severity))
            .then(b.triggered_at_ms.cmp(&a.triggered_at_ms))
    });
}

fn severity_rank(s: AlertSeverity) -> u8 {
    match s {
        AlertSeverity::Critical => 2,
        AlertSeverity::Warning => 1,
    }
}

/// Compute the per-agent score + deployment roll-up. Pulled out so unit
/// tests can exercise the scorer without standing up the bridge.
pub async fn compute_health_summary(
    query: &MetricsQuery,
    engine: &AlertEngine,
    enforcer: Option<&Arc<BudgetEnforcer>>,
    window_hours: u32,
) -> Result<HealthSummary, super::query::MetricsQueryError> {
    let agents = query.list_agents(window_hours)?;
    let active = engine.active_alerts();
    let budget_status = match enforcer {
        Some(e) => Some(e.status().await),
        None => None,
    };
    let mut rows = Vec::with_capacity(agents.len());
    let mut total_cost: u64 = 0;
    let mut total_invocations: u64 = 0;
    let mut total_errors: u64 = 0;
    for s in &agents {
        let active_for_agent = active.iter().filter(|a| a.agent == s.agent).count() as u64;
        let avg_confidence = query
            .avg_confidence_for(&s.agent, window_hours)
            .ok()
            .flatten();
        let budget_util = budget_status
            .as_ref()
            .and_then(|status| status.agents.iter().find(|r| r.agent == s.agent))
            .and_then(|row| {
                row.daily_limit_micros.and_then(|limit| {
                    if limit == 0 {
                        None
                    } else {
                        Some((row.daily_actual_micros as f64 / limit as f64) * 100.0)
                    }
                })
            });
        let score = compute_score(
            s.error_rate * 100.0,
            s.p95_latency_ms,
            avg_confidence,
            budget_util,
            active_for_agent,
        );
        let status = status_for(score);
        rows.push(AgentHealthRow {
            agent: s.agent.clone(),
            score,
            status,
            error_rate_pct: s.error_rate * 100.0,
            p95_latency_ms: s.p95_latency_ms,
            avg_confidence,
            daily_budget_utilization_pct: budget_util,
            active_alerts: active_for_agent,
        });
        total_cost = total_cost.saturating_add(s.total_cost_micros);
        total_invocations = total_invocations.saturating_add(s.invocations);
        total_errors = total_errors.saturating_add(s.errors);
    }
    let overall_error_rate_pct = if total_invocations > 0 {
        (total_errors as f64 / total_invocations as f64) * 100.0
    } else {
        0.0
    };
    let avg_score = if rows.is_empty() {
        0
    } else {
        let sum: u32 = rows.iter().map(|r| r.score).sum();
        sum / rows.len() as u32
    };
    Ok(HealthSummary {
        agents: rows,
        deployment: DeploymentHealthSummary {
            total_cost_usd: total_cost as f64 / 1_000_000.0,
            total_invocations,
            overall_error_rate_pct,
            active_alert_count: active.len() as u64,
            avg_health_score: avg_score,
        },
        window_hours,
    })
}

/// Composite health score in `[0, 100]`. Weights:
///
/// - 40% error rate (linear penalty up to 25% error → 0 points; capped).
/// - 30% p95 latency vs a soft baseline of 2000 ms (linear; 10s → 0).
/// - 20% confidence trend (1.0 = full credit; 0.0 = no credit).
///   When no confidence data exists the dimension contributes the full
///   weight — operators who haven't wired §7.19 don't get penalised.
/// - 10% budget utilisation (linear penalty as utilisation climbs from
///   0% → 100%; over-budget caps at 0). When no budget configured the
///   dimension contributes the full weight.
///
/// A high error rate ALSO scales the entire score by a sliding
/// reliability multiplier so a >25% error rate alone forces the agent
/// into red even when latency / confidence / budget are healthy. Every
/// currently-active alert subtracts a flat 5 points after the
/// multiplier (capped at 0).
pub fn compute_score(
    error_rate_pct: f64,
    p95_latency_ms: u64,
    avg_confidence: Option<f64>,
    daily_budget_utilization_pct: Option<f64>,
    active_alerts: u64,
) -> u32 {
    let err_factor = 1.0 - (error_rate_pct.clamp(0.0, 25.0) / 25.0);
    let lat_factor = 1.0 - ((p95_latency_ms.min(10_000) as f64 - 2_000.0).max(0.0) / 8_000.0);
    let conf_factor = avg_confidence.map(|c| c.clamp(0.0, 1.0)).unwrap_or(1.0);
    let budget_factor = daily_budget_utilization_pct
        .map(|u| 1.0 - u.clamp(0.0, 100.0) / 100.0)
        .unwrap_or(1.0);

    let weighted =
        40.0 * err_factor + 30.0 * lat_factor + 20.0 * conf_factor + 10.0 * budget_factor;
    // Reliability multiplier — a deeply broken agent (>=50% errors)
    // can't sit in the yellow band just because everything else looks
    // fine. Drops linearly from 1.0 at 0% errors to 0 at 50% errors.
    let reliability = (1.0 - (error_rate_pct.clamp(0.0, 50.0) / 50.0)).max(0.0);
    let raw = (weighted * reliability) - (active_alerts as f64) * 5.0;
    raw.clamp(0.0, 100.0) as u32
}

fn status_for(score: u32) -> String {
    if score >= 80 {
        "green".into()
    } else if score >= 50 {
        "yellow".into()
    } else {
        "red".into()
    }
}

/// Static descriptor list for the three observability capabilities.
pub fn observability_capability_descriptors() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "observability.active_alerts",
            "RELIX-7.28 Part 2: every currently-firing alert (ErrorRate, P95Latency, \
             CostPerHour, ZeroSuccess, LowConfidence, BudgetExceeded) sorted by severity then \
             recency. Empty array when nothing is firing.",
        ),
        (
            "observability.alert_history",
            "RELIX-7.28 Part 2: recent rows from the alert chronicle. Args (JSON): \
             {limit?, agent?}. limit defaults to 50; agent filters to one agent.",
        ),
        (
            "observability.health_summary",
            "RELIX-7.28 Part 2: per-agent health score (0–100) weighted across error rate, \
             p95 latency, confidence trend, and budget utilisation. Args (JSON): {hours?} \
             (default 24). Returns {agents: [...], deployment: {...}, window_hours}.",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::alert::{AlertEngine, AlertKind, AlertThresholds};
    use crate::metrics::store::MetricsStore;
    use crate::metrics::types::InvocationMetric;

    fn metric(
        agent: &str,
        method: &str,
        ts_ms: i64,
        latency: u64,
        success: bool,
        cost: Option<u64>,
        confidence: Option<f32>,
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
            input_bytes: 10,
            output_bytes: 20,
            model: None,
            confidence_score: confidence,
            routing_tier: None,
            request_id: None,
        }
    }

    #[test]
    fn score_full_when_everything_healthy() {
        let s = compute_score(0.0, 100, Some(1.0), Some(10.0), 0);
        assert!(s >= 95, "expected full health, got {s}");
    }

    #[test]
    fn high_error_rate_produces_low_score() {
        let s = compute_score(50.0, 100, Some(1.0), Some(0.0), 0);
        assert!(s < 50, "expected red score, got {s}");
        assert_eq!(status_for(s), "red");
    }

    #[test]
    fn low_confidence_trend_reduces_score() {
        let high = compute_score(0.0, 100, Some(1.0), Some(0.0), 0);
        let low = compute_score(0.0, 100, Some(0.0), Some(0.0), 0);
        assert!(
            high > low,
            "lower confidence must yield lower score: {high} vs {low}"
        );
    }

    #[test]
    fn budget_above_eighty_percent_reduces_score() {
        let healthy = compute_score(0.0, 100, Some(1.0), Some(10.0), 0);
        let stressed = compute_score(0.0, 100, Some(1.0), Some(85.0), 0);
        assert!(
            healthy > stressed,
            "higher budget utilisation must lower score: {healthy} vs {stressed}"
        );
    }

    #[test]
    fn active_alerts_subtract_from_score() {
        let no_alerts = compute_score(0.0, 100, Some(1.0), Some(0.0), 0);
        let with_alerts = compute_score(0.0, 100, Some(1.0), Some(0.0), 3);
        assert_eq!(no_alerts.saturating_sub(with_alerts), 15);
    }

    #[test]
    fn score_clamps_to_zero_when_overpenalised() {
        let s = compute_score(50.0, 10_000, Some(0.0), Some(100.0), 20);
        assert_eq!(s, 0);
    }

    #[tokio::test]
    async fn compute_health_summary_handles_no_agents() {
        let store = MetricsStore::in_memory().unwrap();
        let q = MetricsQuery::new(store);
        let engine = AlertEngine::new(q.clone(), AlertThresholds::default());
        let summary = compute_health_summary(&q, &engine, None, 24).await.unwrap();
        assert!(summary.agents.is_empty());
        assert_eq!(summary.deployment.total_invocations, 0);
        assert_eq!(summary.deployment.avg_health_score, 0);
    }

    #[tokio::test]
    async fn compute_health_summary_returns_one_row_per_agent() {
        let store = MetricsStore::in_memory().unwrap();
        let now = crate::metrics::collector::now_ms();
        for _ in 0..20 {
            store
                .insert(&metric(
                    "alice",
                    "ai.chat",
                    now,
                    100,
                    true,
                    Some(1_000),
                    Some(0.9),
                ))
                .unwrap();
        }
        let q = MetricsQuery::new(store);
        let engine = AlertEngine::new(q.clone(), AlertThresholds::default());
        let summary = compute_health_summary(&q, &engine, None, 24).await.unwrap();
        assert_eq!(summary.agents.len(), 1);
        let row = &summary.agents[0];
        assert_eq!(row.agent, "alice");
        assert!(row.score > 0);
        assert_eq!(row.active_alerts, 0);
    }

    #[test]
    fn sort_alerts_puts_critical_before_warning() {
        let mut alerts = vec![
            ActiveAlert {
                agent: "alice".into(),
                kind: AlertKind::ErrorRate,
                severity: AlertSeverity::Warning,
                triggered_at_ms: 100,
                threshold: 10.0,
                actual: 11.0,
                message: "warn".into(),
                method: None,
            },
            ActiveAlert {
                agent: "bob".into(),
                kind: AlertKind::CostPerHour,
                severity: AlertSeverity::Critical,
                triggered_at_ms: 50,
                threshold: 1.0,
                actual: 2.0,
                message: "crit".into(),
                method: None,
            },
        ];
        sort_alerts_for_dashboard(&mut alerts);
        assert_eq!(alerts[0].severity, AlertSeverity::Critical);
    }

    #[test]
    fn status_strings_match_documented_buckets() {
        assert_eq!(status_for(95), "green");
        assert_eq!(status_for(60), "yellow");
        assert_eq!(status_for(10), "red");
    }

    #[test]
    fn descriptors_cover_three_capabilities() {
        let descs = observability_capability_descriptors();
        let methods: Vec<&str> = descs.iter().map(|(m, _)| *m).collect();
        assert_eq!(methods.len(), 3);
        assert!(methods.contains(&"observability.active_alerts"));
        assert!(methods.contains(&"observability.alert_history"));
        assert!(methods.contains(&"observability.health_summary"));
    }
}
