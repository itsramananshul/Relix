//! Dispatch handlers for the spine objects — the `mandate.*` and
//! `campaign.*` capabilities.
//!
//! Wire format mirrors the Task (`Brief`) handlers: pipe-delimited
//! UTF-8 args, the tenant taken from the [`InvocationCtx`] (never
//! from the args), structured reads returned as JSON. Registered
//! via [`register`] from the coordinator controller alongside the
//! Task handlers.
//!
//! Every write is **tenant-guarded**: an update first confirms the
//! object belongs to the caller's tenant (the underlying store's
//! `update_*` is id-keyed, so the guard is what stops a caller in
//! tenant A from mutating tenant B's Mandate/Campaign).

use std::sync::Arc;

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

use super::store::{SpineStore, SpineStoreError};
// Reuse the coordinator's error-envelope helpers (visible to this
// descendant module).
use super::super::{internal, invalid};

/// Register the `mandate.*` and `campaign.*` capabilities on the
/// dispatch bridge. Call once from the coordinator controller with
/// the shared [`SpineStore`].
pub fn register(bridge: &mut DispatchBridge, store: Arc<SpineStore>) {
    macro_rules! cap {
        ($method:literal, $handler:path) => {{
            let s = store.clone();
            bridge.register(
                $method,
                Arc::new(FnHandler(move |ctx: InvocationCtx| {
                    let s = s.clone();
                    async move { $handler(&s, &ctx) }
                })),
            );
        }};
    }

    cap!("mandate.create", handle_mandate_create);
    cap!("mandate.get", handle_mandate_get);
    cap!("mandate.list", handle_mandate_list);
    cap!("mandate.update", handle_mandate_update);
    cap!("campaign.create", handle_campaign_create);
    cap!("campaign.get", handle_campaign_get);
    cap!("campaign.list", handle_campaign_list);
    cap!("campaign.update", handle_campaign_update);
}

// ── mandate.* ─────────────────────────────────────────────

/// `mandate.create` — args `title|description|owner_agent_id|parent_mandate_id`.
/// Only `title` is required; the rest are optional. Tenant from ctx.
/// Returns the new `mandate_id` as the body.
fn handle_mandate_create(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("mandate.create utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(4, '|').collect();
    let title = parts.first().copied().unwrap_or("").trim();
    if title.is_empty() {
        return invalid(
            "mandate.create: title required (title|description|owner|parent)".to_string(),
        );
    }
    let description = parts.get(1).copied().unwrap_or("");
    let owner = parts.get(2).copied().filter(|v| !v.trim().is_empty());
    let parent = parts.get(3).copied().filter(|v| !v.trim().is_empty());
    match store.create_mandate(ctx.tenant_id_or_default(), title, description, owner, parent) {
        Ok(id) => HandlerOutcome::Ok(id.into_bytes()),
        Err(SpineStoreError::BadInput(m)) => invalid(format!("mandate.create: {m}")),
        Err(e) => internal(format!("mandate.create: {e}")),
    }
}

/// `mandate.get` — args `mandate_id`. Tenant-scoped. Returns the
/// Mandate as JSON.
fn handle_mandate_get(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("mandate.get utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("mandate.get: mandate_id required".to_string());
    }
    match store.get_mandate_for_tenant(id, ctx.tenant_id_or_default()) {
        Ok(Some(m)) => match serde_json::to_vec(&m) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("mandate.get encode: {e}")),
        },
        Ok(None) => invalid(format!("mandate.get: not found: {id}")),
        Err(e) => internal(format!("mandate.get: {e}")),
    }
}

/// `mandate.list` — args `status_filter` (optional). Tenant-scoped.
/// Returns a JSON array of Mandates, newest first.
fn handle_mandate_list(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("mandate.list utf8: {e}")),
    };
    let status = if raw.is_empty() { None } else { Some(raw) };
    match store.list_mandates(ctx.tenant_id_or_default(), status) {
        Ok(rows) => match serde_json::to_vec(&rows) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("mandate.list encode: {e}")),
        },
        Err(e) => internal(format!("mandate.list: {e}")),
    }
}

/// `mandate.update` — args `mandate_id|field|value`. Tenant-guarded.
fn handle_mandate_update(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("mandate.update utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(3, '|').collect();
    if parts.len() < 3 {
        return invalid("mandate.update: expected `mandate_id|field|value`".to_string());
    }
    let id = parts[0].trim();
    // Tenant guard: refuse to touch a Mandate outside the caller's tenant.
    match store.get_mandate_for_tenant(id, ctx.tenant_id_or_default()) {
        Ok(Some(_)) => {}
        Ok(None) => return invalid(format!("mandate.update: not found in tenant: {id}")),
        Err(e) => return internal(format!("mandate.update: {e}")),
    }
    match store.update_mandate_field(id, parts[1].trim(), parts[2]) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(SpineStoreError::BadInput(m)) => invalid(format!("mandate.update: {m}")),
        Err(SpineStoreError::NotFound(m)) => invalid(format!("mandate.update: not found: {m}")),
        Err(e) => internal(format!("mandate.update: {e}")),
    }
}

// ── campaign.* ────────────────────────────────────────────

/// `campaign.create` — args `title|mandate_id|lead_agent_id|workspace`.
/// Only `title` is required. Tenant from ctx. Returns the new id.
fn handle_campaign_create(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("campaign.create utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(4, '|').collect();
    let title = parts.first().copied().unwrap_or("").trim();
    if title.is_empty() {
        return invalid(
            "campaign.create: title required (title|mandate|lead|workspace)".to_string(),
        );
    }
    let mandate = parts.get(1).copied().filter(|v| !v.trim().is_empty());
    let lead = parts.get(2).copied().filter(|v| !v.trim().is_empty());
    let workspace = parts.get(3).copied().filter(|v| !v.trim().is_empty());
    match store.create_campaign(ctx.tenant_id_or_default(), title, mandate, lead, workspace) {
        Ok(id) => HandlerOutcome::Ok(id.into_bytes()),
        Err(SpineStoreError::BadInput(m)) => invalid(format!("campaign.create: {m}")),
        Err(e) => internal(format!("campaign.create: {e}")),
    }
}

/// `campaign.get` — args `campaign_id`. Tenant-scoped. JSON body.
fn handle_campaign_get(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("campaign.get utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("campaign.get: campaign_id required".to_string());
    }
    match store.get_campaign_for_tenant(id, ctx.tenant_id_or_default()) {
        Ok(Some(c)) => match serde_json::to_vec(&c) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("campaign.get encode: {e}")),
        },
        Ok(None) => invalid(format!("campaign.get: not found: {id}")),
        Err(e) => internal(format!("campaign.get: {e}")),
    }
}

/// `campaign.list` — args `mandate_filter` (optional). Tenant-scoped.
/// JSON array, newest first.
fn handle_campaign_list(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("campaign.list utf8: {e}")),
    };
    let mandate = if raw.is_empty() { None } else { Some(raw) };
    match store.list_campaigns(ctx.tenant_id_or_default(), mandate) {
        Ok(rows) => match serde_json::to_vec(&rows) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("campaign.list encode: {e}")),
        },
        Err(e) => internal(format!("campaign.list: {e}")),
    }
}

/// `campaign.update` — args `campaign_id|field|value`. Tenant-guarded.
fn handle_campaign_update(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("campaign.update utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(3, '|').collect();
    if parts.len() < 3 {
        return invalid("campaign.update: expected `campaign_id|field|value`".to_string());
    }
    let id = parts[0].trim();
    match store.get_campaign_for_tenant(id, ctx.tenant_id_or_default()) {
        Ok(Some(_)) => {}
        Ok(None) => return invalid(format!("campaign.update: not found in tenant: {id}")),
        Err(e) => return internal(format!("campaign.update: {e}")),
    }
    match store.update_campaign_field(id, parts[1].trim(), parts[2]) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(SpineStoreError::BadInput(m)) => invalid(format!("campaign.update: {m}")),
        Err(SpineStoreError::NotFound(m)) => invalid(format!("campaign.update: not found: {m}")),
        Err(e) => internal(format!("campaign.update: {e}")),
    }
}
