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
    cap!("mandate.children", handle_mandate_children);
    cap!("mandate.tree", handle_mandate_tree);
    cap!("mandate.search", handle_mandate_search);
    cap!("mandate.update", handle_mandate_update);
    cap!("campaign.create", handle_campaign_create);
    cap!("campaign.get", handle_campaign_get);
    cap!("campaign.list", handle_campaign_list);
    cap!("campaign.search", handle_campaign_search);
    cap!("campaign.update", handle_campaign_update);
    cap!("guild.get", handle_guild_get);
    cap!("guild.counts", handle_guild_counts);
    cap!("guild.set", handle_guild_set);
    cap!("guild.set_allowance", handle_guild_set_allowance);
    cap!("guild.set_billing_code", handle_guild_set_billing_code);
    cap!("mandate.propose_strategy", handle_mandate_propose_strategy);
    cap!("mandate.approve_strategy", handle_mandate_approve_strategy);
    cap!("mandate.reject_strategy", handle_mandate_reject_strategy);
    cap!("mandate.strategy", handle_mandate_strategy);
}

// ── mandate strategy gate (Phase 4) ──────────────────────

/// A single trimmed id arg, or an INVALID_ARGS outcome.
fn one_id<'a>(ctx: &'a InvocationCtx, method: &str) -> Result<&'a str, HandlerOutcome> {
    let raw = std::str::from_utf8(&ctx.args)
        .map_err(|e| invalid(format!("{method} utf8: {e}")))?
        .trim();
    if raw.is_empty() {
        return Err(invalid(format!("{method}: id required")));
    }
    Ok(raw)
}

/// `mandate.propose_strategy` — propose a strategy. Arg `mandate_id|doc`.
fn handle_mandate_propose_strategy(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("mandate.propose_strategy utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(2, '|').collect();
    if parts.is_empty() || parts[0].trim().is_empty() {
        return invalid("mandate.propose_strategy: expected `mandate_id|doc`".to_string());
    }
    let doc = parts.get(1).copied().unwrap_or("");
    match store.propose_strategy(ctx.tenant_id_or_default(), parts[0].trim(), doc) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(SpineStoreError::BadInput(m)) => invalid(format!("mandate.propose_strategy: {m}")),
        Err(e) => internal(format!("mandate.propose_strategy: {e}")),
    }
}

/// `mandate.approve_strategy` — approve. Arg `mandate_id`. Tenant-guarded.
fn handle_mandate_approve_strategy(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match one_id(ctx, "mandate.approve_strategy") {
        Ok(i) => i,
        Err(o) => return o,
    };
    match store.approve_strategy(ctx.tenant_id_or_default(), id) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(SpineStoreError::NotFound(m)) => invalid(format!("mandate.approve_strategy: {m}")),
        Err(SpineStoreError::BadInput(m)) => invalid(format!("mandate.approve_strategy: {m}")),
        Err(e) => internal(format!("mandate.approve_strategy: {e}")),
    }
}

/// `mandate.reject_strategy` — reject. Arg `mandate_id`. Tenant-guarded.
fn handle_mandate_reject_strategy(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match one_id(ctx, "mandate.reject_strategy") {
        Ok(i) => i,
        Err(o) => return o,
    };
    match store.reject_strategy(ctx.tenant_id_or_default(), id) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(SpineStoreError::NotFound(m)) => invalid(format!("mandate.reject_strategy: {m}")),
        Err(SpineStoreError::BadInput(m)) => invalid(format!("mandate.reject_strategy: {m}")),
        Err(e) => internal(format!("mandate.reject_strategy: {e}")),
    }
}

/// `mandate.strategy` — the strategy status word, or empty if none.
/// Arg `mandate_id`. Tenant-guarded.
fn handle_mandate_strategy(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match one_id(ctx, "mandate.strategy") {
        Ok(i) => i,
        Err(o) => return o,
    };
    match store.strategy_status(ctx.tenant_id_or_default(), id) {
        Ok(Some(s)) => HandlerOutcome::Ok(s.into_bytes()),
        Ok(None) => HandlerOutcome::Ok(Vec::new()),
        Err(SpineStoreError::BadInput(m)) => invalid(format!("mandate.strategy: {m}")),
        Err(e) => internal(format!("mandate.strategy: {e}")),
    }
}

// ── guild.* ───────────────────────────────────────────────

/// `guild.get` — read the caller's Guild (display name) as JSON, or
/// empty body when unnamed. Tenant from ctx.
fn handle_guild_get(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    match store.get_guild(ctx.tenant_id_or_default()) {
        Ok(Some(g)) => match serde_json::to_vec(&g) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("guild.get encode: {e}")),
        },
        Ok(None) => HandlerOutcome::Ok(Vec::new()),
        Err(e) => internal(format!("guild.get: {e}")),
    }
}

/// `guild.counts` — the Guild's spine at a glance (Mandate &
/// Campaign totals + in-flight subset) as JSON. No args. Tenant
/// from ctx.
fn handle_guild_counts(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    match store.guild_counts(ctx.tenant_id_or_default()) {
        Ok(counts) => match serde_json::to_vec(&counts) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("guild.counts encode: {e}")),
        },
        Err(e) => internal(format!("guild.counts: {e}")),
    }
}

/// `guild.set` — set the caller's Guild display name. Arg `display_name`.
fn handle_guild_set(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let name = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("guild.set utf8: {e}")),
    };
    match store.set_guild_name(ctx.tenant_id_or_default(), name) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(SpineStoreError::BadInput(m)) => invalid(format!("guild.set: {m}")),
        Err(e) => internal(format!("guild.set: {e}")),
    }
}

/// `guild.set_allowance` — set the Guild's monthly Allowance in
/// cents. Arg `cents` (empty clears the cap). Tenant from ctx.
fn handle_guild_set_allowance(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("guild.set_allowance utf8: {e}")),
    };
    let cents = if raw.is_empty() {
        None
    } else {
        match raw.parse::<i64>() {
            Ok(c) => Some(c),
            Err(_) => {
                return invalid(format!("guild.set_allowance: not an integer: {raw}"));
            }
        }
    };
    match store.set_guild_allowance(ctx.tenant_id_or_default(), cents) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(SpineStoreError::BadInput(m)) => invalid(format!("guild.set_allowance: {m}")),
        Err(e) => internal(format!("guild.set_allowance: {e}")),
    }
}

/// `guild.set_billing_code` — set the Guild's OBJECT-LEVEL billing code
/// (company-model §6.6). Arg `code` (empty clears it). Tenant from ctx.
/// Mirrors `guild.set_allowance`.
fn handle_guild_set_billing_code(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("guild.set_billing_code utf8: {e}")),
    };
    let code = if raw.is_empty() { None } else { Some(raw) };
    match store.set_guild_billing_code(ctx.tenant_id_or_default(), code) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(SpineStoreError::BadInput(m)) => invalid(format!("guild.set_billing_code: {m}")),
        Err(e) => internal(format!("guild.set_billing_code: {e}")),
    }
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
    match store.create_mandate(
        ctx.tenant_id_or_default(),
        title,
        description,
        owner,
        parent,
    ) {
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

/// `mandate.search` — args `query|limit` (limit default 50).
/// Tenant-scoped. Returns a JSON array of Mandates whose title
/// contains the query (literal substring), newest first.
fn handle_mandate_search(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("mandate.search utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(2, '|').collect();
    let query = parts.first().copied().unwrap_or("").trim();
    if query.is_empty() {
        return invalid("mandate.search: query required".to_string());
    }
    let limit: usize = parts
        .get(1)
        .copied()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    match store.search_mandates(ctx.tenant_id_or_default(), query, limit) {
        Ok(rows) => match serde_json::to_vec(&rows) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("mandate.search encode: {e}")),
        },
        Err(e) => internal(format!("mandate.search: {e}")),
    }
}

/// `mandate.tree` — args `mandate_id`. Tenant-scoped. Returns the
/// Mandate with its direct sub-Mandates + Campaigns as one JSON
/// object; `not found` when the Mandate isn't in the caller's Guild.
fn handle_mandate_tree(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("mandate.tree utf8: {e}")),
    };
    if raw.is_empty() {
        return invalid("mandate.tree: mandate_id required".to_string());
    }
    match store.mandate_tree(ctx.tenant_id_or_default(), raw) {
        Ok(Some(t)) => match serde_json::to_vec(&t) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("mandate.tree encode: {e}")),
        },
        Ok(None) => invalid(format!("mandate.tree: not found: {raw}")),
        Err(e) => internal(format!("mandate.tree: {e}")),
    }
}

/// `mandate.children` — args `parent_mandate_id`. Tenant-scoped.
/// Returns a JSON array of the parent's direct child Mandates,
/// newest first — the nested-Mandate drill-down.
fn handle_mandate_children(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("mandate.children utf8: {e}")),
    };
    if raw.is_empty() {
        return invalid("mandate.children: parent_mandate_id required".to_string());
    }
    match store.list_child_mandates(ctx.tenant_id_or_default(), raw) {
        Ok(rows) => match serde_json::to_vec(&rows) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("mandate.children encode: {e}")),
        },
        Err(e) => internal(format!("mandate.children: {e}")),
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

/// `campaign.search` — args `query|limit` (limit default 50).
/// Tenant-scoped. JSON array of Campaigns whose title contains the
/// query (literal substring), newest first.
fn handle_campaign_search(store: &SpineStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("campaign.search utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(2, '|').collect();
    let query = parts.first().copied().unwrap_or("").trim();
    if query.is_empty() {
        return invalid("campaign.search: query required".to_string());
    }
    let limit: usize = parts
        .get(1)
        .copied()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    match store.search_campaigns(ctx.tenant_id_or_default(), query, limit) {
        Ok(rows) => match serde_json::to_vec(&rows) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("campaign.search encode: {e}")),
        },
        Err(e) => internal(format!("campaign.search: {e}")),
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
