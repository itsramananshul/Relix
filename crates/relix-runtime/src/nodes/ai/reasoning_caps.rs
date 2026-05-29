//! GAP 16 Components 2 + 4 — operator-callable reasoning caps.
//!
//! Three caps registered on the AI controller's dispatch bridge
//! when the matching `[ai.reasoning.*]` section turns them on:
//!
//! - `ai.judge_eval` (Component 4) — run the 5-question judge
//!   prompt against the configured judge model and return the
//!   parsed verdict.
//! - `ai.self_consistency` (Component 2 signal) — dispatch the
//!   same prompt N times to the configured provider, cluster
//!   the answers, and return the modal answer + consistency
//!   score.
//! - `ai.confidence_aggregate` (Component 2 aggregator) — pure
//!   3-signal aggregator. Operators call this after assembling
//!   the three signals themselves.
//!
//! All three caps take JSON args + return JSON bodies so the
//! bridge proxy can pass them through verbatim.

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::Deserialize;

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use crate::nodes::ai::provider::{ChatInput, ChatProvider};
use crate::nodes::ai::reasoning::confidence_signals::{
    ThreeSignalConfidence, cluster_self_consistency_samples,
};
use crate::nodes::ai::reasoning::judge::{build_judge_prompt, build_verdict, parse_judge_response};
use crate::nodes::ai::reasoning::{JudgeConfig, SelfConsistencyConfig};

/// Register every reasoning cap on `bridge`.
pub fn register(
    bridge: &mut DispatchBridge,
    provider: Arc<dyn ChatProvider>,
    default_model: String,
    judge_cfg: JudgeConfig,
    self_consistency_cfg: SelfConsistencyConfig,
) {
    register_judge(bridge, provider.clone(), default_model.clone(), judge_cfg);
    register_self_consistency(bridge, provider, default_model, self_consistency_cfg);
    register_aggregate(bridge);
}

#[derive(Debug, Deserialize)]
struct JudgeArgs {
    user_prompt: String,
    agent_answer: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    threshold: Option<u32>,
}

fn register_judge(
    bridge: &mut DispatchBridge,
    provider: Arc<dyn ChatProvider>,
    default_model: String,
    cfg: JudgeConfig,
) {
    let provider = provider.clone();
    bridge.register(
        "ai.judge_eval",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let provider = provider.clone();
            let default_model = default_model.clone();
            let cfg = cfg.clone();
            async move { handle_judge(provider, default_model, cfg, ctx).await }
        })),
    );
}

async fn handle_judge(
    provider: Arc<dyn ChatProvider>,
    default_model: String,
    cfg: JudgeConfig,
    ctx: InvocationCtx,
) -> HandlerOutcome {
    let args: JudgeArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("ai.judge_eval: {e}")),
    };
    if args.user_prompt.trim().is_empty() || args.agent_answer.trim().is_empty() {
        return invalid_args("ai.judge_eval: user_prompt and agent_answer required".into());
    }
    let model = args
        .model
        .filter(|m| !m.trim().is_empty())
        .or_else(|| {
            if cfg.model.trim().is_empty() {
                None
            } else {
                Some(cfg.model.clone())
            }
        })
        .unwrap_or(default_model);
    let threshold = args.threshold.unwrap_or(cfg.threshold).max(1);

    let prompt = build_judge_prompt(&args.user_prompt, &args.agent_answer);
    let input = ChatInput {
        session_id: format!("judge-{}", hex::encode(ctx.request_id.0)),
        prompt,
        history: String::new(),
        model,
        system_prompt: None,
        temperature: Some(0.0),
        max_tokens: Some(512),
        thinking_budget_tokens: None,
    };
    let output = match provider.generate_reply(input).await {
        Ok(o) => o,
        Err(e) => return responder_internal(format!("ai.judge_eval: provider: {e}")),
    };
    let flags = parse_judge_response(&output.text);
    let verdict = build_verdict(flags, threshold);
    let body = serde_json::json!({
        "model": output.model,
        "verdict": verdict,
    });
    json_ok(&body)
}

#[derive(Debug, Deserialize)]
struct SelfConsistencyArgs {
    prompt: String,
    #[serde(default)]
    sample_count: Option<u32>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    system_prompt: Option<String>,
}

fn register_self_consistency(
    bridge: &mut DispatchBridge,
    provider: Arc<dyn ChatProvider>,
    default_model: String,
    cfg: SelfConsistencyConfig,
) {
    bridge.register(
        "ai.self_consistency",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let provider = provider.clone();
            let default_model = default_model.clone();
            let cfg = cfg.clone();
            async move { handle_self_consistency(provider, default_model, cfg, ctx).await }
        })),
    );
}

async fn handle_self_consistency(
    provider: Arc<dyn ChatProvider>,
    default_model: String,
    cfg: SelfConsistencyConfig,
    ctx: InvocationCtx,
) -> HandlerOutcome {
    let args: SelfConsistencyArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("ai.self_consistency: {e}")),
    };
    if args.prompt.trim().is_empty() {
        return invalid_args("ai.self_consistency: prompt required".into());
    }
    let n = args.sample_count.unwrap_or(cfg.sample_count).clamp(2, 10) as usize;
    let model = args
        .model
        .filter(|m| !m.trim().is_empty())
        .unwrap_or(default_model);
    let mut samples = Vec::with_capacity(n);
    for _ in 0..n {
        let input = ChatInput {
            session_id: format!("sc-{}", hex::encode(ctx.request_id.0)),
            prompt: args.prompt.clone(),
            history: String::new(),
            model: model.clone(),
            system_prompt: args.system_prompt.clone(),
            temperature: Some(0.7),
            max_tokens: Some(512),
            thinking_budget_tokens: None,
        };
        match provider.generate_reply(input).await {
            Ok(out) => samples.push(out.text),
            Err(e) => {
                return responder_internal(format!("ai.self_consistency: provider: {e}"));
            }
        }
    }
    let outcome = cluster_self_consistency_samples(&samples);
    let score = outcome.score();
    let body = serde_json::json!({
        "outcome": outcome,
        "score": score,
        "model": model,
    });
    json_ok(&body)
}

#[derive(Debug, Deserialize)]
struct AggregateArgs {
    #[serde(default)]
    self_consistency: Option<f32>,
    #[serde(default)]
    retrieval_quality: Option<f32>,
    #[serde(default)]
    judge_passes_of_five: Option<u32>,
}

fn register_aggregate(bridge: &mut DispatchBridge) {
    bridge.register(
        "ai.confidence_aggregate",
        Arc::new(FnHandler(move |ctx: InvocationCtx| async move {
            handle_aggregate(&ctx)
        })),
    );
}

fn handle_aggregate(ctx: &InvocationCtx) -> HandlerOutcome {
    let args: AggregateArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("ai.confidence_aggregate: {e}")),
    };
    let agg = ThreeSignalConfidence;
    let score = agg.aggregate(
        args.self_consistency,
        args.retrieval_quality,
        args.judge_passes_of_five,
    );
    json_ok(&score)
}

fn invalid_args(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

fn responder_internal(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause,
        retry_hint: 1,
        retry_after: None,
    })
}

fn json_ok<T: serde::Serialize>(value: &T) -> HandlerOutcome {
    match serde_json::to_vec(value) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => responder_internal(format!("encode response: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};

    fn ctx(args: serde_json::Value) -> InvocationCtx {
        InvocationCtx {
            request_id: RequestId([1u8; 16]),
            trace_id: TraceId([1u8; 16]),
            caller: VerifiedIdentity {
                subject_id: NodeId([0u8; 32]),
                name: "test".into(),
                org_id: NodeId([0u8; 32]),
                groups: vec![],
                role: "test".into(),
                clearance: "internal".into(),
                bundle_id: [0u8; 32],
            },
            args: serde_json::to_vec(&args).unwrap(),
            tenant_id: None,
        }
    }

    #[test]
    fn confidence_aggregate_returns_score_and_band_from_three_signals() {
        let c = ctx(serde_json::json!({
            "self_consistency": 1.0,
            "retrieval_quality": 1.0,
            "judge_passes_of_five": 5,
        }));
        let outcome = handle_aggregate(&c);
        match outcome {
            HandlerOutcome::Ok(body) => {
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(v["band"].as_str(), Some("high"));
                let score = v["score"].as_f64().unwrap();
                assert!(score > 0.99);
            }
            HandlerOutcome::Err(e) => panic!("aggregate failed: {} {}", e.kind, e.cause),
        }
    }

    #[test]
    fn aggregate_rejects_unparseable_body_with_invalid_args() {
        let c = InvocationCtx {
            request_id: RequestId([0u8; 16]),
            trace_id: TraceId([0u8; 16]),
            caller: VerifiedIdentity {
                subject_id: NodeId([0u8; 32]),
                name: "t".into(),
                org_id: NodeId([0u8; 32]),
                groups: vec![],
                role: "test".into(),
                clearance: "internal".into(),
                bundle_id: [0u8; 32],
            },
            args: b"not-json".to_vec(),
            tenant_id: None,
        };
        match handle_aggregate(&c) {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            _ => panic!("expected INVALID_ARGS"),
        }
    }
}
