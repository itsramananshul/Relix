//! RELIX-7.28 Part 1 — coordinator-side wiring for budget.* capabilities.
//!
//! Registers two unary capabilities:
//!
//! - `budget.status` — returns the configured per-agent + deployment caps
//!   alongside the live accumulated cost (refreshed via the enforcer's
//!   in-memory cache + a backing query against the metrics store).
//! - `budget.reset` — clears the enforcer's cache for a (agent, window)
//!   pair so the next `check` re-reads from the store. Used for
//!   incident recovery and tests.

use std::sync::Arc;

use serde::Deserialize;

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use relix_core::types::{ErrorEnvelope, error_kinds};

use super::budget::{BudgetEnforcer, Window, parse_window};

#[derive(Debug, Default, Deserialize)]
struct ResetArgs {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    window: String,
}

pub fn register(bridge: &mut DispatchBridge, enforcer: Arc<BudgetEnforcer>) {
    register_status(bridge, enforcer.clone());
    register_reset(bridge, enforcer);
}

fn register_status(bridge: &mut DispatchBridge, enforcer: Arc<BudgetEnforcer>) {
    bridge.register(
        "budget.status",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| {
            let enforcer = enforcer.clone();
            async move {
                let status = enforcer.status().await;
                match serde_json::to_vec(&status) {
                    Ok(b) => HandlerOutcome::Ok(b),
                    Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::RESPONDER_INTERNAL,
                        cause: format!("budget.status encode: {e}"),
                        retry_hint: 0,
                        retry_after: None,
                    }),
                }
            }
        })),
    );
}

fn register_reset(bridge: &mut DispatchBridge, enforcer: Arc<BudgetEnforcer>) {
    bridge.register(
        "budget.reset",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let enforcer = enforcer.clone();
            async move {
                let args: ResetArgs = if ctx.args.is_empty() {
                    return invalid("budget.reset: JSON args {agent?, window} required");
                } else {
                    match serde_json::from_slice(&ctx.args) {
                        Ok(a) => a,
                        Err(e) => return invalid(&format!("budget.reset decode: {e}")),
                    }
                };
                let window = match parse_window(&args.window) {
                    Some(w) => w,
                    None => return invalid("budget.reset: window must be 'daily' or 'hourly'"),
                };
                enforcer.reset(args.agent.as_deref(), window);
                let descriptor = match (args.agent.as_deref(), window) {
                    (Some(a), Window::Daily) => format!("agent {a} / daily"),
                    (Some(a), Window::Hourly) => format!("agent {a} / hourly"),
                    (None, Window::Daily) => "deployment / daily".to_string(),
                    (None, Window::Hourly) => "deployment / hourly".to_string(),
                };
                let body = serde_json::json!({
                    "ok": true,
                    "reset": descriptor,
                });
                match serde_json::to_vec(&body) {
                    Ok(b) => HandlerOutcome::Ok(b),
                    Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::RESPONDER_INTERNAL,
                        cause: format!("budget.reset encode: {e}"),
                        retry_hint: 0,
                        retry_after: None,
                    }),
                }
            }
        })),
    );
}

fn invalid(msg: &str) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause: msg.to_string(),
        retry_hint: 0,
        retry_after: None,
    })
}

/// Static descriptor list for the two budget capabilities.
pub fn budget_capability_descriptors() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "budget.status",
            "RELIX-7.28 Part 1: snapshot of every configured agent's accumulated daily + hourly \
             cost, plus the deployment-wide totals when configured. JSON shape: \
             {agents: [...], deployment?: {...}}.",
        ),
        (
            "budget.reset",
            "RELIX-7.28 Part 1: clear the BudgetEnforcer's in-memory cache for an agent+window \
             so the next dispatched call re-reads from the metrics store. Args (JSON): \
             {agent?, window}. window ∈ {\"daily\", \"hourly\"}; omitting `agent` resets the \
             deployment-level cache. Used for incident recovery and test seams.",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::budget::{AgentBudget, BudgetAction, BudgetConfig};
    use crate::metrics::query::MetricsQuery;
    use crate::metrics::store::MetricsStore;

    fn fresh_enforcer() -> Arc<BudgetEnforcer> {
        let store = MetricsStore::in_memory().unwrap();
        let q = MetricsQuery::new(store);
        Arc::new(BudgetEnforcer::new(
            BudgetConfig {
                agents: vec![AgentBudget {
                    agent: "alice".into(),
                    daily_limit_usd: Some(1.0),
                    hourly_limit_usd: Some(0.1),
                    action_on_exceed: BudgetAction::Throttle,
                }],
                deployment: None,
                throttle_backoff_ms: 2000,
                cache_refresh_secs: 60,
                exempt_methods: vec![],
            },
            Some(q),
        ))
    }

    #[test]
    fn descriptors_cover_both_capabilities() {
        let descs = budget_capability_descriptors();
        let methods: Vec<&str> = descs.iter().map(|(m, _)| *m).collect();
        assert!(methods.contains(&"budget.status"));
        assert!(methods.contains(&"budget.reset"));
    }

    #[tokio::test]
    async fn enforcer_status_is_serialisable_through_the_capability() {
        let enf = fresh_enforcer();
        enf.set_cached_for_test("agent:alice", crate::metrics::BudgetWindow::Daily, 500_000);
        let body = enf.status().await;
        let bytes = serde_json::to_vec(&body).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let agents = parsed.get("agents").and_then(|v| v.as_array()).unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["agent"], "alice");
        assert_eq!(agents[0]["daily_actual_micros"], 500_000);
    }
}
