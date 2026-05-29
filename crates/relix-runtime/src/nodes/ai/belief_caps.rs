//! GAP 16 Component 3 — `memory.belief_*` capability surface.
//!
//! Registers six caps on the AI controller's dispatch bridge
//! when `[ai.reasoning.belief] enabled = true`:
//!
//! - `memory.belief_add`
//! - `memory.belief_list`
//! - `memory.belief_list_needs_resolution`
//! - `memory.belief_list_conflicts`
//! - `memory.belief_resolve_conflict`
//! - `memory.belief_purge_session`
//!
//! Each cap takes a JSON arg + returns a JSON body so the
//! bridge proxy + CLI can pass everything through verbatim.

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::Deserialize;

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use crate::nodes::ai::reasoning::{BeliefStore, BeliefStoreError};

/// Register every `memory.belief_*` cap on `bridge`.
pub fn register(bridge: &mut DispatchBridge, store: Arc<BeliefStore>) {
    register_add(bridge, store.clone());
    register_list(bridge, store.clone());
    register_list_needs_resolution(bridge, store.clone());
    register_list_conflicts(bridge, store.clone());
    register_resolve_conflict(bridge, store.clone());
    register_purge_session(bridge, store);
}

#[derive(Debug, Deserialize)]
struct AddArgs {
    session_id: String,
    claim: String,
    #[serde(default = "default_confidence")]
    confidence: f32,
    #[serde(default)]
    sources: Vec<String>,
}

fn default_confidence() -> f32 {
    0.7
}

fn register_add(bridge: &mut DispatchBridge, store: Arc<BeliefStore>) {
    let handler_store = store;
    bridge.register(
        "memory.belief_add",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = handler_store.clone();
            async move { handle_add(&store, &ctx) }
        })),
    );
}

fn handle_add(store: &BeliefStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: AddArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.belief_add: {e}")),
    };
    if args.session_id.trim().is_empty() {
        return invalid_args("memory.belief_add: session_id required".into());
    }
    if args.claim.trim().is_empty() {
        return invalid_args("memory.belief_add: claim required".into());
    }
    match store.add_or_reinforce(
        &args.session_id,
        &args.claim,
        args.confidence,
        &args.sources,
        unix_now_ms(),
    ) {
        Ok(b) => json_ok(&b),
        Err(e) => internal(format!("memory.belief_add: {e}")),
    }
}

#[derive(Debug, Deserialize)]
struct SessionArgs {
    session_id: String,
    #[serde(default)]
    floor: Option<f32>,
}

fn register_list(bridge: &mut DispatchBridge, store: Arc<BeliefStore>) {
    let handler_store = store;
    bridge.register(
        "memory.belief_list",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = handler_store.clone();
            async move { handle_list(&store, &ctx) }
        })),
    );
}

fn handle_list(store: &BeliefStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: SessionArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.belief_list: {e}")),
    };
    match store.list_for_session(&args.session_id) {
        Ok(rows) => json_ok(&rows),
        Err(e) => internal(format!("memory.belief_list: {e}")),
    }
}

fn register_list_needs_resolution(bridge: &mut DispatchBridge, store: Arc<BeliefStore>) {
    let handler_store = store;
    bridge.register(
        "memory.belief_list_needs_resolution",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = handler_store.clone();
            async move { handle_list_needs_resolution(&store, &ctx) }
        })),
    );
}

fn handle_list_needs_resolution(store: &BeliefStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: SessionArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.belief_list_needs_resolution: {e}")),
    };
    let floor = args.floor.unwrap_or(0.5);
    match store.list_needs_resolution(&args.session_id, floor) {
        Ok(rows) => json_ok(&rows),
        Err(e) => internal(format!("memory.belief_list_needs_resolution: {e}")),
    }
}

fn register_list_conflicts(bridge: &mut DispatchBridge, store: Arc<BeliefStore>) {
    let handler_store = store;
    bridge.register(
        "memory.belief_list_conflicts",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = handler_store.clone();
            async move { handle_list_conflicts(&store, &ctx) }
        })),
    );
}

fn handle_list_conflicts(store: &BeliefStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: SessionArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.belief_list_conflicts: {e}")),
    };
    match store.list_conflicts(&args.session_id) {
        Ok(rows) => json_ok(&rows),
        Err(e) => internal(format!("memory.belief_list_conflicts: {e}")),
    }
}

#[derive(Debug, Deserialize)]
struct ResolveArgs {
    conflict_id: String,
    winner_belief_id: String,
    loser_belief_id: String,
}

fn register_resolve_conflict(bridge: &mut DispatchBridge, store: Arc<BeliefStore>) {
    let handler_store = store;
    bridge.register(
        "memory.belief_resolve_conflict",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = handler_store.clone();
            async move { handle_resolve_conflict(&store, &ctx) }
        })),
    );
}

fn handle_resolve_conflict(store: &BeliefStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: ResolveArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.belief_resolve_conflict: {e}")),
    };
    match store.resolve_conflict(
        &args.conflict_id,
        &args.winner_belief_id,
        &args.loser_belief_id,
        unix_now_ms(),
    ) {
        Ok(()) => HandlerOutcome::Ok(
            serde_json::to_vec(&serde_json::json!({ "ok": true })).unwrap_or_default(),
        ),
        Err(e) => internal(format!("memory.belief_resolve_conflict: {e}")),
    }
}

fn register_purge_session(bridge: &mut DispatchBridge, store: Arc<BeliefStore>) {
    let handler_store = store;
    bridge.register(
        "memory.belief_purge_session",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = handler_store.clone();
            async move { handle_purge(&store, &ctx) }
        })),
    );
}

fn handle_purge(store: &BeliefStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: SessionArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("memory.belief_purge_session: {e}")),
    };
    match store.purge_session(&args.session_id) {
        Ok(()) => HandlerOutcome::Ok(
            serde_json::to_vec(&serde_json::json!({ "ok": true })).unwrap_or_default(),
        ),
        Err(e) => internal(format!("memory.belief_purge_session: {e}")),
    }
}

fn invalid_args(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

fn internal(cause: String) -> HandlerOutcome {
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
        Err(e) => internal(format!("encode response: {e}")),
    }
}

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Conversion helper kept for trait-bound bridging — callers
/// just receive `BeliefStoreError` strings via `e.to_string()`.
#[allow(dead_code)]
fn err_to_string(e: BeliefStoreError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};

    fn ctx(args: serde_json::Value) -> InvocationCtx {
        let bytes = serde_json::to_vec(&args).unwrap();
        InvocationCtx {
            request_id: RequestId([0u8; 16]),
            trace_id: TraceId([0u8; 16]),
            caller: VerifiedIdentity {
                subject_id: NodeId([0u8; 32]),
                name: "test".into(),
                org_id: NodeId([0u8; 32]),
                groups: vec![],
                role: "test".into(),
                clearance: "internal".into(),
                bundle_id: [0u8; 32],
            },
            args: bytes,
            tenant_id: None,
        }
    }

    #[test]
    fn add_then_list_round_trips_through_handlers() {
        let store = BeliefStore::open_in_memory().unwrap();
        let add_ctx = ctx(serde_json::json!({
            "session_id": "s",
            "claim": "Project deadline: Friday",
            "confidence": 0.8,
            "sources": ["user"],
        }));
        match handle_add(&store, &add_ctx) {
            HandlerOutcome::Ok(_) => {}
            HandlerOutcome::Err(e) => panic!("add failed: kind={} cause={}", e.kind, e.cause),
        }
        let list_ctx = ctx(serde_json::json!({ "session_id": "s" }));
        match handle_list(&store, &list_ctx) {
            HandlerOutcome::Ok(body) => {
                let v: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
                assert_eq!(v.len(), 1);
                assert!(v[0]["claim"].as_str().unwrap().contains("Friday"));
            }
            HandlerOutcome::Err(e) => panic!("list failed: kind={} cause={}", e.kind, e.cause),
        }
    }

    #[test]
    fn add_rejects_empty_session_or_claim() {
        let store = BeliefStore::open_in_memory().unwrap();
        let ctx_empty_sess = ctx(serde_json::json!({
            "session_id": "",
            "claim": "x",
        }));
        match handle_add(&store, &ctx_empty_sess) {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            _ => panic!("expected invalid_args"),
        }
        let ctx_empty_claim = ctx(serde_json::json!({
            "session_id": "s",
            "claim": "",
        }));
        match handle_add(&store, &ctx_empty_claim) {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            _ => panic!("expected invalid_args"),
        }
    }

    #[test]
    fn resolve_conflict_round_trips_through_handler() {
        let store = BeliefStore::open_in_memory().unwrap();
        handle_add(
            &store,
            &ctx(serde_json::json!({
                "session_id": "s",
                "claim": "Reporting frequency: weekly",
                "confidence": 0.7,
            })),
        );
        handle_add(
            &store,
            &ctx(serde_json::json!({
                "session_id": "s",
                "claim": "Reporting frequency: monthly",
                "confidence": 0.6,
            })),
        );
        let conflicts = store.list_conflicts("s").unwrap();
        assert_eq!(conflicts.len(), 1);
        let beliefs = store.list_for_session("s").unwrap();
        let winner = &beliefs[0];
        let loser = &beliefs[1];
        let resolve_ctx = ctx(serde_json::json!({
            "conflict_id": conflicts[0].id,
            "winner_belief_id": winner.id,
            "loser_belief_id": loser.id,
        }));
        match handle_resolve_conflict(&store, &resolve_ctx) {
            HandlerOutcome::Ok(_) => {}
            HandlerOutcome::Err(e) => {
                panic!("resolve failed: kind={} cause={}", e.kind, e.cause)
            }
        }
        assert!(store.list_conflicts("s").unwrap().is_empty());
    }
}
