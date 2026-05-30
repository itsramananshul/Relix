//! Coordinator caps for the §7.30 PART 2 credential vault.

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::Deserialize;

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

use super::store::{CredentialKind, CredentialStore};

/// Wire every `credentials.*` cap onto `bridge`. Always
/// registered; operator authorisation lives at policy time
/// via the existing capability policy engine. The `get` cap
/// additionally enforces caller == owner_agent in-handler so
/// even a permissive policy can't leak a credential to a
/// non-owner.
pub fn register(bridge: &mut DispatchBridge, store: CredentialStore) {
    {
        let s = store.clone();
        bridge.register(
            "credentials.store",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_store(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "credentials.get",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_get(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "credentials.rotate",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_rotate(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "credentials.revoke",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_revoke(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "credentials.list",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_list(&s, &ctx) }
            })),
        );
    }
    {
        bridge.register(
            "credentials.audit",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = store.clone();
                async move { handle_audit(&s, &ctx) }
            })),
        );
    }
}

#[derive(Debug, Deserialize)]
struct StoreArgs {
    name: String,
    value: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    owner_agent: Option<String>,
    #[serde(default)]
    expires_at_ms: Option<i64>,
    #[serde(default)]
    rotation_interval_secs: Option<u64>,
}

fn handle_store(store: &CredentialStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: StoreArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.name.trim().is_empty() || args.value.is_empty() {
        return invalid("name and value are required");
    }
    let kind = args
        .kind
        .as_deref()
        .map(CredentialKind::parse)
        .unwrap_or_default();
    let actor = ctx.caller.name.clone();
    let result = if store.tenant_isolation_enabled() {
        store.store_for_tenant(
            &args.name,
            &args.value,
            kind,
            args.owner_agent.as_deref(),
            args.expires_at_ms,
            args.rotation_interval_secs,
            Some(&actor),
            ctx.tenant_id.as_deref(),
        )
    } else {
        store.store(
            &args.name,
            &args.value,
            kind,
            args.owner_agent.as_deref(),
            args.expires_at_ms,
            args.rotation_interval_secs,
            Some(&actor),
        )
    };
    match result {
        Ok(c) => ok_json(&super::store::CredentialSummary::from(&c)),
        Err(e) => internal(&format!("{e}")),
    }
}

#[derive(Debug, Deserialize)]
struct NameArgs {
    name: String,
}

fn handle_get(store: &CredentialStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: NameArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.name.trim().is_empty() {
        return invalid("name is required");
    }
    // Lookup the row first so we can authorisation-check
    // caller vs owner_agent before decrypting.
    let lookup = if store.tenant_isolation_enabled() {
        store.list_for_tenant(None, ctx.tenant_id.as_deref())
    } else {
        store.list(None)
    };
    let summary = match lookup {
        Ok(rows) => rows.into_iter().find(|r| r.name == args.name),
        Err(e) => return internal(&format!("{e}")),
    };
    let Some(summary) = summary else {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!("credentials: unknown name `{}`", args.name),
            retry_hint: 0,
            retry_after: None,
        });
    };
    let caller = &ctx.caller.name;
    let is_owner = summary.owner_agent.as_deref() == Some(caller.as_str());
    let is_operator = ctx
        .caller
        .groups
        .iter()
        .any(|g| g == "operators" || g == "admin");
    if !(is_owner || is_operator || summary.owner_agent.is_none()) {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::SECURITY_DENIED,
            cause: format!(
                "credentials: caller `{caller}` is not the owner of `{}`",
                args.name
            ),
            retry_hint: 0,
            retry_after: None,
        });
    }
    let decrypted = if store.tenant_isolation_enabled() {
        store.get_for_tenant(&args.name, Some(caller), ctx.tenant_id.as_deref())
    } else {
        store.get(&args.name, Some(caller))
    };
    match decrypted {
        Ok(Some(plain)) => ok_json(&plain),
        Ok(None) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!("credentials: `{}` is revoked or expired", args.name),
            retry_hint: 0,
            retry_after: None,
        }),
        Err(e) => internal(&format!("{e}")),
    }
}

#[derive(Debug, Deserialize)]
struct RotateArgs {
    name: String,
    new_value: String,
}

fn handle_rotate(store: &CredentialStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: RotateArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.name.trim().is_empty() || args.new_value.is_empty() {
        return invalid("name and new_value are required");
    }
    let actor = ctx.caller.name.clone();
    match store.rotate(&args.name, &args.new_value, Some(&actor)) {
        Ok(c) => ok_json(&super::store::CredentialSummary::from(&c)),
        Err(e) => internal(&format!("{e}")),
    }
}

#[derive(Debug, Deserialize)]
struct RevokeArgs {
    name: String,
    #[serde(default)]
    reason: Option<String>,
}

fn handle_revoke(store: &CredentialStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: RevokeArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.name.trim().is_empty() {
        return invalid("name is required");
    }
    let actor = ctx.caller.name.clone();
    match store.revoke(&args.name, args.reason.as_deref(), Some(&actor)) {
        Ok(c) => ok_json(&super::store::CredentialSummary::from(&c)),
        Err(e) => internal(&format!("{e}")),
    }
}

#[derive(Debug, Deserialize, Default)]
struct ListArgs {
    #[serde(default)]
    owner_agent: Option<String>,
}

fn handle_list(store: &CredentialStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: ListArgs = match decode_optional(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    let result = if store.tenant_isolation_enabled() {
        store.list_for_tenant(args.owner_agent.as_deref(), ctx.tenant_id.as_deref())
    } else {
        store.list(args.owner_agent.as_deref())
    };
    match result {
        Ok(rows) => ok_json(&rows),
        Err(e) => internal(&format!("{e}")),
    }
}

#[derive(Debug, Deserialize)]
struct AuditArgs {
    name: String,
    #[serde(default)]
    limit: Option<usize>,
}

fn handle_audit(store: &CredentialStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: AuditArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.name.trim().is_empty() {
        return invalid("name is required");
    }
    match store.audit_rows(&args.name, args.limit.unwrap_or(0)) {
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

fn decode_optional<T: serde::de::DeserializeOwned + Default>(
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
            cause: format!("credentials: encode response: {e}"),
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
