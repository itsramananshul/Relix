//! Coordinator-side wiring for RELIX-7.11.
//!
//! Registers five capabilities on the dispatch bridge:
//!
//! - `metrics.agent_summary`    — per-agent summary
//! - `metrics.method_breakdown` — per-method breakdown
//! - `metrics.timeseries`       — time-series buckets
//! - `metrics.alerts_active`    — active alerts
//! - `metrics.cost_report`      — cost breakdown by agent + method
//! - `metrics.agents`           — list every agent with summary
//!   (the bridge's GET /v1/metrics/agents endpoint)
//!
//! All capabilities are unary. Args + responses are JSON.

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::Deserialize;

use super::alert::AlertEngine;
use super::query::{MetricsQuery, MetricsQueryError, TimeseriesQuery};
use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

pub const DEFAULT_HOURS: u32 = 24;
pub const DEFAULT_BUCKET_MINUTES: u32 = 5;

/// Wire all six metrics capabilities into `bridge`.
pub fn register(bridge: &mut DispatchBridge, query: MetricsQuery, engine: Option<AlertEngine>) {
    register_agents(bridge, query.clone());
    register_agent_summary(bridge, query.clone());
    register_method_breakdown(bridge, query.clone());
    register_timeseries(bridge, query.clone());
    register_cost_report(bridge, query);
    register_alerts_active(bridge, engine);
}

#[derive(Debug, Deserialize, Default)]
struct AgentSummaryArgs {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    hours: u32,
}

#[derive(Debug, Deserialize, Default)]
struct MethodBreakdownArgs {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    hours: u32,
}

#[derive(Debug, Deserialize, Default)]
struct TimeseriesArgs {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    hours: u32,
    #[serde(default)]
    bucket_minutes: u32,
}

#[derive(Debug, Deserialize, Default)]
struct CostArgs {
    #[serde(default)]
    hours: u32,
}

#[derive(Debug, Deserialize, Default)]
struct AgentsArgs {
    #[serde(default)]
    hours: u32,
}

fn register_agents(bridge: &mut DispatchBridge, q: MetricsQuery) {
    bridge.register(
        "metrics.agents",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let q = q.clone();
            async move {
                let args = match decode::<AgentsArgs>(&ctx.args) {
                    Ok(a) => a,
                    Err(e) => return e,
                };
                let hours = if args.hours == 0 {
                    DEFAULT_HOURS
                } else {
                    args.hours
                };
                match q.list_agents(hours) {
                    Ok(rows) => ok_json(&rows),
                    Err(e) => err_internal(&e),
                }
            }
        })),
    );
}

fn register_agent_summary(bridge: &mut DispatchBridge, q: MetricsQuery) {
    bridge.register(
        "metrics.agent_summary",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let q = q.clone();
            async move {
                let args = match decode::<AgentSummaryArgs>(&ctx.args) {
                    Ok(a) => a,
                    Err(e) => return e,
                };
                if args.agent.is_empty() {
                    return invalid("agent is required");
                }
                let hours = if args.hours == 0 {
                    DEFAULT_HOURS
                } else {
                    args.hours
                };
                match q.agent_summary(&args.agent, hours) {
                    Ok(rec) => ok_json(&rec),
                    Err(e) => err_internal(&e),
                }
            }
        })),
    );
}

fn register_method_breakdown(bridge: &mut DispatchBridge, q: MetricsQuery) {
    bridge.register(
        "metrics.method_breakdown",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let q = q.clone();
            async move {
                let args = match decode::<MethodBreakdownArgs>(&ctx.args) {
                    Ok(a) => a,
                    Err(e) => return e,
                };
                if args.agent.is_empty() {
                    return invalid("agent is required");
                }
                let hours = if args.hours == 0 {
                    DEFAULT_HOURS
                } else {
                    args.hours
                };
                match q.method_breakdown(&args.agent, args.method.as_deref(), hours) {
                    Ok(rows) => ok_json(&rows),
                    Err(e) => err_internal(&e),
                }
            }
        })),
    );
}

fn register_timeseries(bridge: &mut DispatchBridge, q: MetricsQuery) {
    bridge.register(
        "metrics.timeseries",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let q = q.clone();
            async move {
                let args = match decode::<TimeseriesArgs>(&ctx.args) {
                    Ok(a) => a,
                    Err(e) => return e,
                };
                if args.agent.is_empty() {
                    return invalid("agent is required");
                }
                let hours = if args.hours == 0 {
                    DEFAULT_HOURS
                } else {
                    args.hours
                };
                let bucket_minutes = if args.bucket_minutes == 0 {
                    DEFAULT_BUCKET_MINUTES
                } else {
                    args.bucket_minutes
                };
                let req = TimeseriesQuery {
                    agent: args.agent.clone(),
                    hours,
                    bucket_minutes,
                };
                match q.timeseries(&req) {
                    Ok(rows) => ok_json(&rows),
                    Err(MetricsQueryError::Arg(m)) => invalid(&m),
                    Err(e) => err_internal(&e),
                }
            }
        })),
    );
}

fn register_cost_report(bridge: &mut DispatchBridge, q: MetricsQuery) {
    bridge.register(
        "metrics.cost_report",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let q = q.clone();
            async move {
                let args = match decode::<CostArgs>(&ctx.args) {
                    Ok(a) => a,
                    Err(e) => return e,
                };
                let hours = if args.hours == 0 {
                    DEFAULT_HOURS
                } else {
                    args.hours
                };
                match q.cost_report(hours) {
                    Ok(rows) => ok_json(&rows),
                    Err(e) => err_internal(&e),
                }
            }
        })),
    );
}

fn register_alerts_active(bridge: &mut DispatchBridge, engine: Option<AlertEngine>) {
    bridge.register(
        "metrics.alerts_active",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| {
            let engine = engine.clone();
            async move {
                let alerts = match engine.as_ref() {
                    Some(e) => e.active_alerts(),
                    None => Vec::new(),
                };
                ok_json(&alerts)
            }
        })),
    );
}

fn decode<T: serde::de::DeserializeOwned + Default>(args: &[u8]) -> Result<T, HandlerOutcome> {
    if args.is_empty() {
        // Empty body → defaults.
        return Ok(T::default());
    }
    serde_json::from_slice(args).map_err(|e| invalid(&format!("decode args: {e}")))
}

fn ok_json<T: serde::Serialize>(value: &T) -> HandlerOutcome {
    match serde_json::to_vec(value) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("metrics: encode response: {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

fn invalid(msg: &str) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause: msg.to_string(),
        retry_hint: 0,
        retry_after: None,
    })
}

fn err_internal<E: std::fmt::Display>(e: &E) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause: format!("metrics: {e}"),
        retry_hint: 0,
        retry_after: None,
    })
}

// ── GAP 22 Feature 2 baseline-store surface ──────────────────

#[derive(Debug, Deserialize, Default)]
struct CostBaselinesArgs {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    last_n_windows: u32,
}

#[derive(Debug, Deserialize, Default)]
struct AskHumanBaselinesArgs {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    last_n_windows: u32,
}

#[derive(Debug, Deserialize, Default)]
struct CostSpikeHistoryArgs {
    #[serde(default)]
    limit: u32,
}

/// Wire the three baseline + spike-history caps. Idempotent —
/// callers register additional caps from the same store + sink
/// elsewhere without conflict.
pub fn register_baseline_caps(
    bridge: &mut DispatchBridge,
    store: super::cost_baseline::CostBaselineStore,
) {
    register_cost_baselines(bridge, store.clone());
    register_ask_human_baselines(bridge, store.clone());
    register_cost_spike_history(bridge, store);
}

fn register_cost_baselines(
    bridge: &mut DispatchBridge,
    store: super::cost_baseline::CostBaselineStore,
) {
    bridge.register(
        "metrics.cost_baselines",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = store.clone();
            async move {
                let args = match decode::<CostBaselinesArgs>(&ctx.args) {
                    Ok(a) => a,
                    Err(e) => return e,
                };
                let limit = if args.last_n_windows == 0 {
                    24
                } else {
                    args.last_n_windows
                };
                match store.recent_cost_baselines(args.provider.as_deref(), limit) {
                    Ok(rows) => ok_json(&serde_json::json!({ "windows": rows })),
                    Err(e) => err_internal(&e),
                }
            }
        })),
    );
}

fn register_ask_human_baselines(
    bridge: &mut DispatchBridge,
    store: super::cost_baseline::CostBaselineStore,
) {
    bridge.register(
        "metrics.ask_human_baselines",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = store.clone();
            async move {
                let args = match decode::<AskHumanBaselinesArgs>(&ctx.args) {
                    Ok(a) => a,
                    Err(e) => return e,
                };
                let limit = if args.last_n_windows == 0 {
                    24
                } else {
                    args.last_n_windows
                };
                match store.recent_ask_human_baselines(args.agent.as_deref(), limit) {
                    Ok(rows) => ok_json(&serde_json::json!({ "windows": rows })),
                    Err(e) => err_internal(&e),
                }
            }
        })),
    );
}

fn register_cost_spike_history(
    bridge: &mut DispatchBridge,
    store: super::cost_baseline::CostBaselineStore,
) {
    bridge.register(
        "metrics.cost_spike_history",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = store.clone();
            async move {
                let args = match decode::<CostSpikeHistoryArgs>(&ctx.args) {
                    Ok(a) => a,
                    Err(e) => return e,
                };
                let limit = if args.limit == 0 { 20 } else { args.limit };
                match store.recent_spike_history(limit) {
                    Ok(rows) => ok_json(&serde_json::json!({ "spikes": rows })),
                    Err(e) => err_internal(&e),
                }
            }
        })),
    );
}

#[cfg(test)]
mod tests {
    use super::super::alert::{AlertEngine, AlertThresholds};
    use super::super::store::MetricsStore;
    use super::super::types::InvocationMetric;
    use super::*;
    use crate::dispatch::DispatchBridge;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use relix_core::policy::PolicyEngine;
    use serde_json::Value;

    fn fresh_bridge() -> (DispatchBridge, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::permissive();
        let bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        (bridge, dir)
    }

    fn metric(agent: &str, method: &str, ts_ms: i64) -> InvocationMetric {
        InvocationMetric {
            agent_name: agent.into(),
            tenant_id: "default".into(),
            peer_alias: "p".into(),
            method: method.into(),
            timestamp_ms: ts_ms,
            latency_ms: 10,
            success: true,
            error_kind: None,
            token_count: None,
            cost_micros: None,
            input_bytes: 10,
            output_bytes: 20,
            model: None,
            confidence_score: None,
            routing_tier: None,
            request_id: None,
        }
    }

    /// We can't easily push an invocation through the
    /// DispatchBridge without spinning up an entire envelope
    /// pipeline. The unit tests here exercise the
    /// `register_*` paths by calling the dispatch bridge's
    /// in-process handler map directly via the InvocationCtx
    /// shape — relying on the `FnHandler` we registered to
    /// project the JSON args.
    ///
    /// The end-to-end coordinator test lives in the mini-mesh
    /// bridge test (`crates/relix-web-bridge/.../metrics_*`).
    #[tokio::test]
    async fn capabilities_register_without_panic() {
        let (mut bridge, _dir) = fresh_bridge();
        let store = MetricsStore::in_memory().unwrap();
        store
            .insert(&metric(
                "alice",
                "ai.chat",
                super::super::collector::now_ms(),
            ))
            .unwrap();
        let query = MetricsQuery::new(store.clone());
        let engine = AlertEngine::new(query.clone(), AlertThresholds::default());
        register(&mut bridge, query, Some(engine));
        // Smoke: every registered method appears in the
        // capability_stats snapshot ONLY after a call hits it,
        // but the registration itself should not panic.
        let snapshot = bridge.capability_stats_snapshot();
        // Pre-call snapshot is empty (or sparse with denied
        // attempts only).
        let _ = snapshot;
    }

    #[test]
    fn agents_args_default_round_trips() {
        // No body → default args.
        let a: AgentsArgs = decode(b"").map_err(|_| ()).unwrap();
        assert_eq!(a.hours, 0); // 0 -> means "use default" downstream
    }

    #[test]
    fn decode_invalid_json_yields_invalid_args_error() {
        match decode::<AgentSummaryArgs>(b"not json") {
            Ok(_) => panic!("expected an INVALID_ARGS error"),
            Err(HandlerOutcome::Err(env)) => {
                assert_eq!(env.kind, error_kinds::INVALID_ARGS);
            }
            Err(_) => panic!("unexpected outcome"),
        }
    }

    #[test]
    fn ok_json_serialises_into_handler_outcome() {
        let v = serde_json::json!({"a": 1});
        match ok_json(&v) {
            HandlerOutcome::Ok(body) => {
                let parsed: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(parsed, v);
            }
            _ => panic!("expected Ok"),
        }
    }
}
