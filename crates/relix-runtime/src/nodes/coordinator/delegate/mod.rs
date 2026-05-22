//! Delegation — one agent spawns another as a subtask.
//!
//! Builds on the existing task ledger:
//!
//! - The parent task already lives in `tasks`.
//! - `delegate.spawn` creates a child task with
//!   `origin_surface = "delegation"`, writes a
//!   `delegated_to` edge from the parent (via the existing
//!   `TaskStore::record_delegated`), flips the parent to
//!   `awaiting_input`, and writes a `task.awaiting` chronicle
//!   event on the parent with the child id.
//! - The executor loop (`executor`) polls for
//!   `origin_surface = "delegation"` + `status = "pending"`
//!   rows, dispatches `ai.chat` with the goal + context, then
//!   flips the child to `completed` / `failed` and the parent
//!   back to `running` with a `delegate.child_completed`
//!   chronicle event so the agent loop polling
//!   `delegate.result` sees the new state.
//!
//! Depth cap (default 3) is enforced by:
//! - Validating the `depth` integer in the wire format —
//!   rejecting `>= max_depth`.
//! - Independently walking the `delegated_to` ancestor chain
//!   via [`crate::nodes::coordinator::TaskStore::delegation_chain_depth`]
//!   — a caller that under-reports `depth` still gets caught.

pub mod executor;
pub mod handlers;

pub use executor::{
    DelegationAiDispatcher, DelegationAiDispatcherCell, DelegationAiMeshDispatcher,
    DelegationAiPeerConfig, DelegationConfig, run_one_tick, spawn_delegation_executor,
};

use std::sync::Arc;

use crate::dispatch::{DispatchBridge, FnHandler, InvocationCtx};
use crate::nodes::coordinator::TaskStore;

/// Register the `delegate.spawn / result / cancel / list`
/// capabilities. The executor is spawned separately by
/// [`spawn_delegation_executor`] when `[coordinator.delegation]`
/// is configured.
pub fn register(bridge: &mut DispatchBridge, store: Arc<TaskStore>, max_depth: usize) {
    {
        let s = store.clone();
        let md = max_depth;
        bridge.register(
            "delegate.spawn",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_spawn(&s, &ctx, md) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "delegate.result",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_result(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "delegate.cancel",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_cancel(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "delegate.list",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handlers::handle_list(&s, &ctx) }
            })),
        );
    }
}
