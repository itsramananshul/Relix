//! Coordinator cap for the §7.18 research-backed identity
//! pipeline.

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::Deserialize;

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

use super::research::ResearchPipeline;

/// Wire `identity.research` onto `bridge`.
pub fn register(bridge: &mut DispatchBridge, pipeline: ResearchPipeline) {
    bridge.register(
        "identity.research",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let pipeline = pipeline.clone();
            async move { handle(&pipeline, &ctx).await }
        })),
    );
}

#[derive(Debug, Deserialize)]
struct ResearchArgs {
    subject_name: String,
    #[serde(default)]
    context: Option<String>,
}

async fn handle(pipeline: &ResearchPipeline, ctx: &InvocationCtx) -> HandlerOutcome {
    if ctx.args.is_empty() {
        return invalid("identity.research: args required");
    }
    let args: ResearchArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid(&format!("identity.research decode args: {e}")),
    };
    if args.subject_name.trim().is_empty() {
        return invalid("identity.research: subject_name is required");
    }
    match pipeline
        .run(&args.subject_name, args.context.as_deref())
        .await
    {
        Ok(result) => match serde_json::to_vec(&result) {
            Ok(body) => HandlerOutcome::Ok(body),
            Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: format!("identity.research encode: {e}"),
                retry_hint: 0,
                retry_after: None,
            }),
        },
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("{e}"),
            retry_hint: 1,
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
