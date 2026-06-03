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
pub fn register(
    bridge: &mut DispatchBridge,
    store: CredentialStore,
    // Optional spine agent store: when present, `credentials.get`
    // additionally enforces the reading Operative's `secret_allowlist`
    // (company-model §5.2C) on top of the vault's owner/tenant gate.
    agent_store: Option<Arc<crate::nodes::coordinator::agent::AgentStore>>,
) {
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
        let a = agent_store.clone();
        bridge.register(
            "credentials.get",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let a = a.clone();
                async move { handle_get(&s, a.as_deref(), &ctx) }
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

fn handle_get(
    store: &CredentialStore,
    agent_store: Option<&crate::nodes::coordinator::agent::AgentStore>,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let args: NameArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.name.trim().is_empty() {
        return invalid("name is required");
    }
    // Per-Operative secret_allowlist gate (company-model §5.2C), layered
    // on top of the vault's owner/tenant gate below. Skipped when no
    // spine agent store is wired or the caller is not an Operative.
    if let Some(astore) = agent_store
        && let Err(out) = crate::nodes::coordinator::agent::handlers::enforce_secret_allowlist(
            astore, ctx, &args.name,
        )
    {
        tracing::warn!(
            secret = %args.name,
            caller = %ctx.caller.name,
            "credentials.get: denied by Operative secret_allowlist"
        );
        return out;
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
    // GATE 2 — fail closed on ownership. Reaching this handler
    // means the dispatch policy gate already admitted the caller
    // for `credentials.get`; this is the additional per-secret
    // ownership scope. Only the credential's named owner may
    // read it. A credential with NO owner is DENIED by default
    // (an unscoped secret is not a free-for-all), and there is
    // deliberately NO hard-coded "operators"/"admin" group
    // bypass — any elevated cross-owner access must be granted
    // by explicit policy on a distinct capability, never by a
    // string literal in this code path.
    let caller = &ctx.caller.name;
    let is_owner = summary.owner_agent.as_deref() == Some(caller.as_str());
    if !is_owner {
        let owner_desc = match summary.owner_agent.as_deref() {
            Some(o) => format!("owned by `{o}`"),
            None => "unscoped (no owner)".to_string(),
        };
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::SECURITY_DENIED,
            cause: format!(
                "credentials: caller `{caller}` is not the owner of `{}` ({owner_desc}); \
                 access denied",
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

#[cfg(test)]
mod gate2_ownership_failclosed_tests {
    use super::*;
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};

    fn ctx_for(caller_name: &str, groups: Vec<String>, name: &str) -> InvocationCtx {
        let args = serde_json::to_vec(&serde_json::json!({ "name": name })).unwrap();
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(caller_name.as_bytes()),
                name: caller_name.to_string(),
                org_id: NodeId::from_pubkey(b"org"),
                groups,
                role: String::new(),
                clearance: String::new(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId([0; 16]),
            args,
            tenant_id: None,
        }
    }

    fn is_security_denied(outcome: &HandlerOutcome) -> bool {
        matches!(
            outcome,
            HandlerOutcome::Err(e) if e.kind == error_kinds::SECURITY_DENIED
        )
    }

    #[test]
    fn no_owner_credential_is_denied_even_for_operators_and_admin_groups() {
        // GATE 2: an unscoped (no-owner) credential must DENY by
        // default. The old code allowed it for everyone via
        // `summary.owner_agent.is_none()`, and additionally let
        // anyone in the "operators"/"admin" groups through via a
        // string-literal bypass. Both are gone.
        let store = CredentialStore::open_in_memory("test-master-secret").unwrap();
        store
            .store(
                "shared-key",
                "s3cr3t",
                CredentialKind::Secret,
                None, // NO owner
                None,
                None,
                Some("seed"),
            )
            .unwrap();

        // Caller is in BOTH legacy-bypass groups — must STILL be denied.
        let ctx = ctx_for(
            "bob",
            vec!["operators".into(), "admin".into()],
            "shared-key",
        );
        let outcome = handle_get(&store, None, &ctx);
        assert!(
            is_security_denied(&outcome),
            "no-owner credential must be denied by default, even for operators/admin groups"
        );
    }

    #[test]
    fn owned_credential_denied_to_non_owner_and_allowed_to_owner() {
        // The fix must not break the gate the other way: the
        // legitimate owner can still read, a non-owner cannot.
        let store = CredentialStore::open_in_memory("test-master-secret").unwrap();
        store
            .store(
                "alice-key",
                "v",
                CredentialKind::Secret,
                Some("alice"),
                None,
                None,
                Some("seed"),
            )
            .unwrap();

        // Non-owner (even in operators group) → denied.
        let bob = ctx_for("bob", vec!["operators".into()], "alice-key");
        assert!(
            is_security_denied(&handle_get(&store, None, &bob)),
            "a non-owner must be denied"
        );

        // Owner → allowed.
        let alice = ctx_for("alice", vec![], "alice-key");
        assert!(
            matches!(handle_get(&store, None, &alice), HandlerOutcome::Ok(_)),
            "the owner must be able to read their own credential"
        );
    }

    #[test]
    fn operative_secret_allowlist_layers_on_top_of_ownership() {
        // company-model §5.2C: even the credential's owner, when it is
        // an Operative, must have the secret in its `secret_allowlist`.
        let store = CredentialStore::open_in_memory("test-master-secret").unwrap();
        store
            .store(
                "alice-key",
                "v",
                CredentialKind::Secret,
                Some("alice"),
                None,
                None,
                Some("seed"),
            )
            .unwrap();
        let agents = crate::nodes::coordinator::agent::AgentStore::in_memory().unwrap();
        // `ctx_for("alice", ..)` carries subject_id = from_pubkey("alice").
        let alice_subject = relix_core::types::NodeId::from_pubkey(b"alice").to_string();
        let alice_id = agents
            .create_agent(
                "alice",
                "engineer",
                "A",
                "e",
                "e",
                "p",
                &alice_subject,
                "medium",
                "default",
            )
            .unwrap();
        let alice = ctx_for("alice", vec![], "alice-key");

        // Owner Operative WITHOUT the secret in its allowlist → denied.
        assert!(
            is_security_denied(&handle_get(&store, Some(&agents), &alice)),
            "owner Operative must still be gated by its secret_allowlist"
        );
        // Grant the secret → now allowed.
        agents
            .update_agent_field(&alice_id, "secret_allowlist", "alice-key")
            .unwrap();
        assert!(
            matches!(
                handle_get(&store, Some(&agents), &alice),
                HandlerOutcome::Ok(_)
            ),
            "an allowlisted owner Operative must be able to read its secret"
        );
    }
}
