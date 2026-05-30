//! Coordinator caps for the §7.30 PART 3 session-identity
//! token service.

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::Deserialize;

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

use super::session::{IssueRequest, SessionIdentityService};

/// Wire all four `identity.*` caps onto `bridge`.
pub fn register(bridge: &mut DispatchBridge, service: SessionIdentityService) {
    {
        let svc = service.clone();
        bridge.register(
            "identity.issue_token",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_issue(&svc, &ctx) }
            })),
        );
    }
    {
        let svc = service.clone();
        bridge.register(
            "identity.verify_token",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_verify(&svc, &ctx) }
            })),
        );
    }
    {
        let svc = service.clone();
        bridge.register(
            "identity.revoke_token",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = svc.clone();
                async move { handle_revoke(&svc, &ctx) }
            })),
        );
    }
    {
        bridge.register(
            "identity.active_tokens",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let svc = service.clone();
                async move { handle_list(&svc, &ctx) }
            })),
        );
    }
}

#[derive(Debug, Deserialize)]
struct IssueArgs {
    session_id: String,
    agent_name: String,
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    ttl_secs: Option<u64>,
}

fn handle_issue(svc: &SessionIdentityService, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: IssueArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.session_id.trim().is_empty() || args.agent_name.trim().is_empty() {
        return invalid("session_id and agent_name are required");
    }
    // Tenant-isolation: when the request omits tenant_id,
    // default to the per-request tenant_id propagated on the
    // InvocationCtx so issued tokens are auto-scoped to the
    // caller's tenant in multi-tenant deployments.
    let req = IssueRequest {
        session_id: args.session_id,
        agent_name: args.agent_name,
        tenant_id: args.tenant_id.or_else(|| ctx.tenant_id.clone()),
        scopes: args.scopes,
        ttl_secs: args.ttl_secs,
    };
    match svc.issue(&req) {
        Ok(tok) => {
            let wire = match tok.to_wire() {
                Ok(w) => w,
                Err(e) => return internal(&format!("encode wire: {e}")),
            };
            let body = serde_json::json!({
                "token": tok,
                "wire": wire,
            });
            ok_json(&body)
        }
        Err(e) => internal(&format!("{e}")),
    }
}

#[derive(Debug, Deserialize)]
struct VerifyArgs {
    token: String,
}

fn handle_verify(svc: &SessionIdentityService, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: VerifyArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.token.trim().is_empty() {
        return invalid("token is required");
    }
    let result = svc.verify(&args.token);
    ok_json(&result)
}

#[derive(Debug, Deserialize)]
struct RevokeArgs {
    session_id: String,
}

fn handle_revoke(svc: &SessionIdentityService, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: RevokeArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.session_id.trim().is_empty() {
        return invalid("session_id is required");
    }
    match svc.revoke(&args.session_id) {
        Ok(n) => ok_json(&serde_json::json!({
            "session_id": args.session_id,
            "revoked_count": n,
        })),
        Err(e) => internal(&format!("{e}")),
    }
}

#[derive(Debug, Deserialize, Default)]
struct ListArgs {
    #[serde(default)]
    agent_name: Option<String>,
}

fn handle_list(svc: &SessionIdentityService, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: ListArgs = if ctx.args.is_empty() {
        ListArgs::default()
    } else {
        match serde_json::from_slice(&ctx.args) {
            Ok(a) => a,
            Err(e) => return invalid(&format!("decode args: {e}")),
        }
    };
    let result = if svc.tenant_isolation_enabled() {
        svc.list_active_for_tenant(args.agent_name.as_deref(), ctx.tenant_id.as_deref())
    } else {
        svc.list_active(args.agent_name.as_deref())
    };
    match result {
        Ok(rows) => ok_json(&rows),
        Err(e) => internal(&format!("{e}")),
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
            cause: format!("identity: encode response: {e}"),
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

fn internal(msg: &str) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause: msg.to_string(),
        retry_hint: 0,
        retry_after: None,
    })
}
