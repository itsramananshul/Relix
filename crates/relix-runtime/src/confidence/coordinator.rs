//! RELIX-7.19 — coordinator-side `confidence.*` dispatch
//! handlers.
//!
//! Three caps, all JSON-encoded:
//!
//! - `confidence.policy_list` — returns every configured policy.
//! - `confidence.score_history` — returns a per-(agent, method)
//!   rolling window snapshot.
//! - `confidence.reset_history` — clears the rolling window for
//!   one (agent, method) or every method on one agent.
//!
//! The handlers borrow a shared [`super::ConfidenceScorer`] +
//! [`super::FallbackEngine`] handle wired in at boot by the
//! controller-runtime. When the bridge has no scorer wired the
//! caps return `INVALID_ARGS` so operators see exactly why
//! the cap is unavailable rather than a silent zero-row
//! result.

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::Deserialize;

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

use super::fallback::FallbackEngine;
use super::scorer::ConfidenceScorer;
use super::self_consistency::SelfConsistencyStats;

/// Wire every `confidence.*` cap onto `bridge`.
pub fn register(
    bridge: &mut DispatchBridge,
    scorer: Arc<ConfidenceScorer>,
    engine: Arc<FallbackEngine>,
    sc_stats: SelfConsistencyStats,
    sc_cfg: super::self_consistency::SelfConsistencyConfig,
) {
    {
        let engine = engine.clone();
        bridge.register(
            "confidence.policy_list",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let engine = engine.clone();
                async move { handle_policy_list(&engine) }
            })),
        );
    }
    {
        let scorer = scorer.clone();
        bridge.register(
            "confidence.score_history",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let scorer = scorer.clone();
                async move { handle_score_history(&scorer, &ctx) }
            })),
        );
    }
    {
        bridge.register(
            "confidence.reset_history",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let scorer = scorer.clone();
                async move { handle_reset_history(&scorer, &ctx) }
            })),
        );
    }
    {
        let sc_stats = sc_stats.clone();
        let sc_cfg = sc_cfg.clone();
        bridge.register(
            "confidence.self_consistency_stats",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let sc_stats = sc_stats.clone();
                let sc_cfg = sc_cfg.clone();
                async move { handle_self_consistency_stats(&sc_stats, &sc_cfg) }
            })),
        );
    }
}

fn handle_self_consistency_stats(
    stats: &SelfConsistencyStats,
    cfg: &super::self_consistency::SelfConsistencyConfig,
) -> HandlerOutcome {
    let snap = stats.snapshot();
    let body = serde_json::json!({
        "config": {
            "enabled": cfg.enabled,
            "sample_count": cfg.sample_count,
            "min_score_to_enable": cfg.min_score_to_enable,
            "capability_patterns": cfg.capability_patterns,
        },
        "stats": snap,
    });
    ok_json(&body)
}

fn handle_policy_list(engine: &FallbackEngine) -> HandlerOutcome {
    let body = engine.list();
    ok_json(&body)
}

#[derive(Debug, Deserialize, Default)]
struct ScoreHistoryArgs {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    method: String,
}

fn handle_score_history(scorer: &ConfidenceScorer, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: ScoreHistoryArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.agent.trim().is_empty() {
        return invalid("agent is required");
    }
    if args.method.trim().is_empty() {
        return invalid("method is required");
    }
    let snap = scorer.snapshot(&args.agent, &args.method);
    ok_json(&snap)
}

#[derive(Debug, Deserialize, Default)]
struct ResetHistoryArgs {
    #[serde(default)]
    agent: String,
    #[serde(default)]
    method: Option<String>,
}

fn handle_reset_history(scorer: &ConfidenceScorer, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: ResetHistoryArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.agent.trim().is_empty() {
        return invalid("agent is required");
    }
    let body = match args.method.as_deref() {
        Some(m) if !m.trim().is_empty() => {
            let cleared = scorer.reset_pair(&args.agent, m);
            serde_json::json!({
                "cleared_pair": cleared,
                "agent": args.agent,
                "method": m,
            })
        }
        _ => {
            let cleared = scorer.reset_agent(&args.agent);
            serde_json::json!({
                "cleared_pairs": cleared,
                "agent": args.agent,
            })
        }
    };
    ok_json(&body)
}

// ── shared helpers ────────────────────────────────────────

fn decode<T: serde::de::DeserializeOwned + Default>(
    ctx: &InvocationCtx,
) -> Result<T, HandlerOutcome> {
    if ctx.args.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(&ctx.args).map_err(|e| invalid(&format!("decode args: {e}")))
}

fn ok_json<T: serde::Serialize>(value: &T) -> HandlerOutcome {
    match serde_json::to_vec(value) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("confidence: encode response: {e}"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confidence::config::{ConfidenceConfig, ConfidencePolicy, FallbackActionConfig};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use relix_core::policy::PolicyEngine;
    use tempfile::TempDir;

    fn fresh_bridge() -> (DispatchBridge, TempDir) {
        let dir = TempDir::new().unwrap();
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

    fn ctx_with(args: &[u8]) -> InvocationCtx {
        use relix_core::identity::VerifiedIdentity;
        use relix_core::types::{NodeId, RequestId, TraceId};
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"caller"),
                name: "alice".into(),
                org_id: NodeId::from_pubkey(b"org"),
                groups: vec!["operators".into()],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: args.to_vec(),
            tenant_id: None,
        }
    }

    fn make_scorer_and_engine() -> (Arc<ConfidenceScorer>, Arc<FallbackEngine>) {
        let cfg = ConfidenceConfig {
            enabled: true,
            policies: vec![ConfidencePolicy {
                capability: "ai.chat".into(),
                low_threshold: 0.5,
                critical_threshold: 0.3,
                low_action: Some(FallbackActionConfig::Retry {
                    max_retries: 2,
                    retry_delay_ms: 100,
                }),
                critical_action: Some(FallbackActionConfig::Abort {
                    abort_message: "x".into(),
                }),
            }],
            ..Default::default()
        };
        let scorer = Arc::new(ConfidenceScorer::from_config(&cfg));
        let engine = Arc::new(FallbackEngine::from_policies(&cfg.policies));
        (scorer, engine)
    }

    #[tokio::test]
    async fn caps_register_without_panic() {
        let (mut bridge, _dir) = fresh_bridge();
        let (scorer, engine) = make_scorer_and_engine();
        register(
            &mut bridge,
            scorer,
            engine,
            crate::confidence::SelfConsistencyStats::new(),
            crate::confidence::SelfConsistencyConfig::default(),
        );
        let _snapshot = bridge.capability_stats_snapshot();
    }

    #[test]
    fn handle_policy_list_returns_resolved_policies() {
        let (_scorer, engine) = make_scorer_and_engine();
        let HandlerOutcome::Ok(body) = handle_policy_list(&engine) else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.is_array());
        assert_eq!(v.as_array().unwrap()[0]["capability"], "ai.chat");
    }

    #[test]
    fn score_history_returns_zero_filled_snapshot_for_unseen_pair() {
        let (scorer, _engine) = make_scorer_and_engine();
        let ctx = ctx_with(br#"{"agent":"a","method":"m"}"#);
        let HandlerOutcome::Ok(body) = handle_score_history(&scorer, &ctx) else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["agent"], "a");
        assert_eq!(v["method"], "m");
        assert_eq!(v["call_count"], 0);
    }

    #[test]
    fn score_history_rejects_missing_agent() {
        let (scorer, _engine) = make_scorer_and_engine();
        let ctx = ctx_with(br#"{"agent":"","method":"m"}"#);
        match handle_score_history(&scorer, &ctx) {
            HandlerOutcome::Err(env) => assert_eq!(env.kind, error_kinds::INVALID_ARGS),
            _ => panic!("expected INVALID_ARGS"),
        }
    }

    #[test]
    fn reset_history_clears_per_pair_and_returns_summary() {
        let (scorer, _engine) = make_scorer_and_engine();
        scorer.record("alice", "ai.chat", false, 50, 0.1);
        scorer.record("alice", "ai.chat", false, 50, 0.1);
        let ctx = ctx_with(br#"{"agent":"alice","method":"ai.chat"}"#);
        let HandlerOutcome::Ok(body) = handle_reset_history(&scorer, &ctx) else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["cleared_pair"], true);
    }

    #[test]
    fn reset_history_without_method_clears_every_method_under_agent() {
        let (scorer, _engine) = make_scorer_and_engine();
        scorer.record("alice", "ai.chat", false, 50, 0.1);
        scorer.record("alice", "ai.embed", false, 50, 0.1);
        let ctx = ctx_with(br#"{"agent":"alice"}"#);
        let HandlerOutcome::Ok(body) = handle_reset_history(&scorer, &ctx) else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["cleared_pairs"], 2);
    }
}
