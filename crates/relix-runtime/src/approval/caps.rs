//! Coordinator caps for the §7.30 PART 1 out-of-band approval
//! delivery surface.

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::Deserialize;

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

use super::delivery::ApprovalDeliveryService;
use super::store::ApprovalStoreError;

/// SEC PART B: roles that may decide any approval, regardless
/// of the per-row `authorized_approvers` allow-list. Stable
/// strings the IdentityBundle wraps in `VerifiedIdentity.role`.
const OPERATOR_ROLES: &[&str] = &["operator", "admin"];

/// Wire the approval-delivery caps onto `bridge`:
///
/// - `approval.delivery_status` (read one row)
/// - `approval.deliver` (dispatch a new approval)
/// - `approval.record_decision` (operator approve / reject)
/// - `approval.failed_deliveries` (PART 6 — list the rows
///   that landed in `delivery_failed` so operators can
///   reconcile via the dashboard / `/v1/approval/failed-deliveries`)
/// - `approval.list_pending` (PART 5 — list rows in `pending`
///   status for the dashboard surface backing
///   `GET /v1/approval/pending`)
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
        let svc = service.clone();
        bridge.register(
            "approval.record_decision",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_record_decision(&svc, &ctx) }
            })),
        );
    }
    {
        let svc = service.clone();
        bridge.register(
            "approval.failed_deliveries",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_failed_deliveries(&svc, &ctx) }
            })),
        );
    }
    {
        bridge.register(
            "approval.list_pending",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = service.clone();
                async move { handle_list_pending(&svc, &ctx) }
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
    /// SEC PART B: optional explicit approver allow-list. When
    /// empty, the bridge's `record_decision` cap falls back to
    /// role-based admission.
    #[serde(default)]
    authorized_approvers: Vec<String>,
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
        authorized_approvers: args.authorized_approvers,
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
    // SEC PART B: authorised-approver check. Fetch the row to
    // get its allow-list, then admit the caller iff:
    //   1. caller is in `authorized_approvers`, OR
    //   2. caller's verified role is in OPERATOR_ROLES.
    // The check runs BEFORE the write so a forged
    // approval_id from an unprivileged agent does not flip
    // any state.
    let row = match service.store().get(&args.approval_id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!(
                    "approval delivery: unknown approval_id `{}`",
                    args.approval_id
                ),
                retry_hint: 0,
                retry_after: None,
            });
        }
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: format!("approval delivery: store read: {e}"),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    let caller_subject = ctx.caller.subject_id.to_string();
    let caller_role = ctx.caller.role.as_str();
    let role_admits = OPERATOR_ROLES.contains(&caller_role);
    let listed = row
        .authorized_approvers
        .iter()
        .any(|s| s == &caller_subject);
    if !role_admits && !listed {
        // Specific cause; never echoes the secret signing key
        // or the row's full approver list (avoids leaking the
        // approver allow-list to an unprivileged caller).
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::SECURITY_DENIED,
            cause: format!(
                "approval delivery: caller `{caller_subject}` is not an \
                 authorised approver for `{}` (role={caller_role})",
                args.approval_id
            ),
            retry_hint: 0,
            retry_after: None,
        });
    }
    match service.record_decision(&args.approval_id, decision, args.note.as_deref()) {
        Ok(()) => ok_json(&serde_json::json!({
            "approval_id": args.approval_id,
            "decision": decision,
        })),
        // SEC PART B: pending-guard violation surfaces as a
        // specific INVALID_ARGS so operators see "already
        // decided" rather than a generic internal error.
        Err(crate::approval::delivery::DeliveryError::Store(
            ApprovalStoreError::AlreadyDecided(id),
        )) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!(
                "approval delivery: approval `{id}` is no longer pending \
                 (already decided)"
            ),
            retry_hint: 0,
            retry_after: None,
        }),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("approval delivery: {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

/// PART 6: list the rows in `delivery_failed` state newest-
/// first. Returns up to `limit` rows; defaults to 50 when
/// args are absent or `limit` is omitted. Capped server-side
/// at 500.
#[derive(Debug, Deserialize, Default)]
struct FailedDeliveriesArgs {
    #[serde(default)]
    limit: Option<usize>,
}

fn handle_failed_deliveries(
    service: &ApprovalDeliveryService,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    // Args are optional — an empty body is equivalent to the
    // default limit.
    let args: FailedDeliveriesArgs = if ctx.args.is_empty() {
        FailedDeliveriesArgs::default()
    } else {
        match serde_json::from_slice(&ctx.args) {
            Ok(v) => v,
            Err(e) => return invalid(&format!("decode args: {e}")),
        }
    };
    let limit = args.limit.unwrap_or(50).clamp(1, 500);
    match service.store().list_failed_deliveries(limit) {
        Ok(rows) => {
            let body = serde_json::json!({
                "count": rows.len(),
                "rows": rows,
            });
            ok_json(&body)
        }
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("approval delivery: list failed-deliveries: {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

/// PART 5: list the rows in `pending` status newest-first.
/// Backs the dashboard's "approvals waiting on me" surface
/// (`GET /v1/approval/pending`). `limit` defaults to 50, capped
/// at 500.
#[derive(Debug, Deserialize, Default)]
struct ListPendingArgs {
    #[serde(default)]
    limit: Option<usize>,
}

fn handle_list_pending(service: &ApprovalDeliveryService, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: ListPendingArgs = if ctx.args.is_empty() {
        ListPendingArgs::default()
    } else {
        match serde_json::from_slice(&ctx.args) {
            Ok(v) => v,
            Err(e) => return invalid(&format!("decode args: {e}")),
        }
    };
    let limit = args.limit.unwrap_or(50).clamp(1, 500);
    match service.store().list(Some("pending"), limit) {
        Ok(rows) => {
            let body = serde_json::json!({
                "count": rows.len(),
                "rows": rows,
            });
            ok_json(&body)
        }
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("approval delivery: list pending: {e}"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::delivery::{
        ApprovalDeliveryConfig, ApprovalDeliveryMatrix, ApprovalDeliveryService, ApprovalRequest,
        ChannelDispatch, ChannelKind, ChannelsConfig, DeliveryError,
    };
    use crate::approval::store::{ApprovalDeliveryRow, ApprovalRequestStore};
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};
    use std::sync::Arc;

    struct NoopDispatch;

    #[async_trait::async_trait]
    impl ChannelDispatch for NoopDispatch {
        async fn send(
            &self,
            _channel: ChannelKind,
            _cfg: &ChannelsConfig,
            _request: &ApprovalRequest,
            _is_escalation: bool,
        ) -> Result<(), DeliveryError> {
            Ok(())
        }
    }

    fn service() -> ApprovalDeliveryService {
        let cfg = ApprovalDeliveryConfig {
            default_channel: "dashboard".into(),
            rules: vec![],
            channels: ChannelsConfig::default(),
        };
        let matrix = ApprovalDeliveryMatrix::new(cfg);
        let store = ApprovalRequestStore::open_in_memory().unwrap();
        ApprovalDeliveryService::new(matrix, store, Arc::new(NoopDispatch))
    }

    fn seed_row(svc: &ApprovalDeliveryService, id: &str, approvers: &[&str]) {
        let row = ApprovalDeliveryRow {
            approval_id: id.into(),
            agent_name: "alice".into(),
            capability: "tool.fs.write".into(),
            request_summary: String::new(),
            session_id: String::new(),
            status: "pending".into(),
            delivery_channel: "dashboard".into(),
            escalated: false,
            escalation_channel: None,
            delivered_at_ms: None,
            escalated_at_ms: None,
            decided_at_ms: None,
            decision: None,
            decision_note: None,
            delivery_error: None,
            authorized_approvers: approvers.iter().map(|s| (*s).to_string()).collect(),
        };
        svc.store().upsert(&row).unwrap();
    }

    fn ctx_for(subject_hex_seed: &[u8], role: &str, args: serde_json::Value) -> InvocationCtx {
        let bytes = serde_json::to_vec(&args).unwrap();
        let mut id = VerifiedIdentity {
            subject_id: NodeId::from_pubkey(subject_hex_seed),
            name: "test".into(),
            org_id: NodeId::from_pubkey(b"org"),
            groups: vec![],
            role: role.into(),
            clearance: String::new(),
            bundle_id: [0; 32],
        };
        // touch role to silence unused-mut on the local
        id.role = role.into();
        InvocationCtx {
            caller: id,
            trace_id: TraceId::new(),
            request_id: RequestId([0; 16]),
            args: bytes,
            tenant_id: None,
        }
    }

    #[tokio::test]
    async fn record_decision_denies_when_caller_not_in_authorized_approvers() {
        // SEC PART B: an agent that knows the approval_id but
        // is not in the allow-list cannot record a decision.
        let svc = service();
        let approver_subject = NodeId::from_pubkey(b"operator-bob").to_string();
        seed_row(&svc, "a-rd-1", &[&approver_subject]);
        let ctx = ctx_for(
            b"random-agent",
            "agent",
            serde_json::json!({
                "approval_id": "a-rd-1",
                "decision": "approved",
                "note": "self-vote",
            }),
        );
        let out = handle_record_decision(&svc, &ctx);
        match out {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, error_kinds::SECURITY_DENIED);
                assert!(
                    env.cause.contains("not an authorised approver"),
                    "got cause: {}",
                    env.cause
                );
            }
            HandlerOutcome::Ok(_) => panic!("unauthorised approval must NOT admit"),
        }
        // Row stays pending.
        let row = svc.store().get("a-rd-1").unwrap().unwrap();
        assert_eq!(row.status, "pending");
    }

    #[tokio::test]
    async fn record_decision_admits_when_caller_in_authorized_approvers() {
        let svc = service();
        let approver_subject = NodeId::from_pubkey(b"operator-bob").to_string();
        seed_row(&svc, "a-rd-2", &[&approver_subject]);
        let ctx = ctx_for(
            b"operator-bob",
            "agent", // role does NOT need to admit; subject list does
            serde_json::json!({
                "approval_id": "a-rd-2",
                "decision": "approved",
                "note": "ok",
            }),
        );
        let out = handle_record_decision(&svc, &ctx);
        assert!(matches!(out, HandlerOutcome::Ok(_)));
        let row = svc.store().get("a-rd-2").unwrap().unwrap();
        assert_eq!(row.status, "approved");
    }

    #[tokio::test]
    async fn record_decision_admits_operator_role_even_when_not_listed() {
        // Operator / admin role overrides per-row allow-list.
        let svc = service();
        let other = NodeId::from_pubkey(b"someone-else").to_string();
        seed_row(&svc, "a-rd-3", &[&other]);
        let ctx = ctx_for(
            b"oncall-operator",
            "operator",
            serde_json::json!({
                "approval_id": "a-rd-3",
                "decision": "approved",
                "note": "operator override",
            }),
        );
        let out = handle_record_decision(&svc, &ctx);
        assert!(matches!(out, HandlerOutcome::Ok(_)));
        let row = svc.store().get("a-rd-3").unwrap().unwrap();
        assert_eq!(row.status, "approved");
    }

    #[tokio::test]
    async fn record_decision_surfaces_already_decided_when_row_not_pending() {
        // SEC PART B: re-decide attempts on a terminal row
        // return the structured INVALID_ARGS cause rather than a
        // silent no-op.
        let svc = service();
        let approver = NodeId::from_pubkey(b"approver").to_string();
        seed_row(&svc, "a-rd-4", &[&approver]);
        let args = serde_json::json!({
            "approval_id": "a-rd-4",
            "decision": "approved",
            "note": "first",
        });
        let ctx1 = ctx_for(b"approver", "agent", args.clone());
        assert!(matches!(
            handle_record_decision(&svc, &ctx1),
            HandlerOutcome::Ok(_)
        ));
        let args2 = serde_json::json!({
            "approval_id": "a-rd-4",
            "decision": "rejected",
            "note": "second",
        });
        let ctx2 = ctx_for(b"approver", "agent", args2);
        match handle_record_decision(&svc, &ctx2) {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("no longer pending"));
            }
            HandlerOutcome::Ok(_) => panic!("expected INVALID_ARGS, got Ok"),
        }
    }

    #[tokio::test]
    async fn record_decision_returns_invalid_args_when_approval_id_unknown() {
        let svc = service();
        let ctx = ctx_for(
            b"operator",
            "operator",
            serde_json::json!({
                "approval_id": "nope",
                "decision": "approved",
            }),
        );
        match handle_record_decision(&svc, &ctx) {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("unknown approval_id"));
            }
            HandlerOutcome::Ok(_) => panic!("expected INVALID_ARGS, got Ok"),
        }
    }
}
