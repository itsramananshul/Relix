//! Coordinator caps for the §7.30 PART 1 out-of-band approval
//! delivery surface.

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::Deserialize;

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

use super::delivery::ApprovalDeliveryService;

/// Wire `approval.delivery_status` (read) +
/// `approval.deliver` (dispatch) onto `bridge`.
pub fn register(bridge: &mut DispatchBridge, service: ApprovalDeliveryService) {
    {
        let svc = service.clone();
        bridge.register(
            "approval.delivery_status",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_status(&svc, &ctx) }
            })),
        );
    }
    {
        let svc = service.clone();
        bridge.register(
            "approval.deliver",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_deliver(&svc, &ctx).await }
            })),
        );
    }
    {
        bridge.register(
            "approval.record_decision",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = service.clone();
                async move { handle_record_decision(&svc, &ctx) }
            })),
        );
    }
}

#[derive(Debug, Deserialize)]
struct StatusArgs {
    approval_id: String,
}

fn handle_status(service: &ApprovalDeliveryService, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: StatusArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.approval_id.trim().is_empty() {
        return invalid("approval_id is required");
    }
    match service.store().get(&args.approval_id) {
        Ok(Some(row)) => {
            let body = serde_json::json!({
                "approval_id": row.approval_id,
                "agent_name": row.agent_name,
                "capability": row.capability,
                "status": row.status,
                "delivery_channel": row.delivery_channel,
                "escalated": row.escalated,
                "escalation_channel": row.escalation_channel,
                "delivered_at_ms": row.delivered_at_ms,
                "escalated_at_ms": row.escalated_at_ms,
                "decided_at_ms": row.decided_at_ms,
                "decision": row.decision,
                "decision_note": row.decision_note,
            });
            ok_json(&body)
        }
        Ok(None) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!(
                "approval delivery: unknown approval_id `{}`",
                args.approval_id
            ),
            retry_hint: 0,
            retry_after: None,
        }),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("approval delivery: store read: {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

#[derive(Debug, Deserialize)]
struct DeliverArgs {
    approval_id: String,
    agent_name: String,
    capability: String,
    #[serde(default)]
    request_summary: String,
    #[serde(default)]
    session_id: String,
}

async fn handle_deliver(service: &ApprovalDeliveryService, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: DeliverArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.approval_id.trim().is_empty()
        || args.agent_name.trim().is_empty()
        || args.capability.trim().is_empty()
    {
        return invalid("approval_id, agent_name, capability are required");
    }
    let request = super::delivery::ApprovalRequest {
        approval_id: args.approval_id.clone(),
        agent_name: args.agent_name,
        capability: args.capability,
        request_summary: args.request_summary,
        session_id: args.session_id,
    };
    match service.dispatch_request(request).await {
        Ok(outcome) => ok_json(&outcome),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("approval delivery: {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

#[derive(Debug, Deserialize)]
struct DecisionArgs {
    approval_id: String,
    decision: String,
    #[serde(default)]
    note: Option<String>,
}

fn handle_record_decision(
    service: &ApprovalDeliveryService,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let args: DecisionArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.approval_id.trim().is_empty() {
        return invalid("approval_id is required");
    }
    let decision = match args.decision.trim().to_ascii_lowercase().as_str() {
        "approved" => "approved",
        "rejected" => "rejected",
        "expired" => "expired",
        _ => return invalid("decision must be one of approved|rejected|expired"),
    };
    match service.record_decision(&args.approval_id, decision, args.note.as_deref()) {
        Ok(()) => ok_json(&serde_json::json!({
            "approval_id": args.approval_id,
            "decision": decision,
        })),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("approval delivery: {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

fn decode<T: serde::de::DeserializeOwned>(ctx: &InvocationCtx) -> Result<T, HandlerOutcome> {
    if ctx.args.is_empty() {
        return Err(invalid("args required"));
    }
    serde_json::from_slice(&ctx.args).map_err(|e| invalid(&format!("decode args: {e}")))
}

fn ok_json<T: serde::Serialize>(value: &T) -> HandlerOutcome {
    match serde_json::to_vec(value) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("approval delivery: encode response: {e}"),
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
