//! GAP 11 — rollback executor + `execution.*` capability
//! handlers.
//!
//! Three JSON-wire caps:
//!
//! - `execution.rollback`        { transaction_id } → RollbackResult
//! - `execution.transaction_get` { transaction_id } → { actions }
//! - `execution.evidence`        { action_id?, actor_id?, limit? } → { records }
//!   (wired in `evidence.rs` — referenced here only so the
//!   capability descriptors land together in
//!   [`register_execution_caps`]).
//!
//! Rollback semantics (matches GAP_REPORT GAP 11 spec):
//!
//! 1. Load every action in the transaction.
//! 2. Walk in **reverse order** (newest first):
//!    - **Tier A** — dispatch `compensating_tool` with
//!      `compensating_args`. Record success/failure. Stamp
//!      `rolled_back = true` on the original row.
//!    - **Tier B** — surface the rollback plan to the operator
//!      via the `human_review_required` list. Do not auto-run.
//!    - **Tier C** — log an error; Tier C should never have
//!      been persisted in the first place.
//! 3. Return the aggregated result.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

use super::gateway_tier::{GatewayTier, RollbackAction, RollbackPlanItem, RollbackResult};
use super::transaction_store::{GatewayActionRow, TransactionStore};

/// Async dispatcher the rollback executor uses to call
/// compensating tools. Production wraps the local mesh client;
/// tests stub it.
#[async_trait::async_trait]
pub trait CompensatingDispatcher: Send + Sync {
    async fn invoke(&self, tool: &str, args_json: &str) -> Result<String, String>;
}

/// Execute the rollback for one transaction. Pure function on
/// the (store, dispatcher) pair — extracted so the cap handler
/// can be a thin wrapper.
pub async fn execute_rollback(
    store: &TransactionStore,
    dispatcher: Option<&dyn CompensatingDispatcher>,
    transaction_id: &str,
) -> Result<RollbackResult, String> {
    let actions = store
        .list_for_transaction(transaction_id)
        .map_err(|e| format!("list transaction: {e}"))?;
    let mut out = RollbackResult {
        auto_rolled_back: Vec::new(),
        human_review_required: Vec::new(),
        errors: Vec::new(),
        transaction_id: transaction_id.to_string(),
    };
    // Newest-first so a multi-step transaction unwinds in
    // reverse — same idiom as a database transaction abort.
    for action in actions.iter().rev() {
        if action.rolled_back {
            // Idempotent: skip what we've already undone.
            continue;
        }
        if !action.success || action.dry_run {
            // Skipped actions / failed actions don't need
            // rollback. The failure already aborted the side
            // effect; dry runs never produced one.
            continue;
        }
        match &action.tier {
            GatewayTier::AutoCompensated {
                compensating_tool,
                compensating_args,
            } => {
                let rollback = run_compensating_call(
                    dispatcher,
                    store,
                    action,
                    compensating_tool,
                    compensating_args,
                )
                .await;
                out.auto_rolled_back.push(rollback);
            }
            GatewayTier::HumanRollbackPlan { rollback_plan } => {
                out.human_review_required.push(RollbackPlanItem {
                    action_id: action.action_id.clone(),
                    tool: action.tool.clone(),
                    rollback_plan: rollback_plan.clone(),
                });
            }
            GatewayTier::Blocked { reason } => {
                out.errors.push(format!(
                    "action `{}` was Tier C (blocked) but persisted anyway — \
                     this is a bug. reason: {reason}",
                    action.action_id
                ));
            }
        }
    }
    Ok(out)
}

async fn run_compensating_call(
    dispatcher: Option<&dyn CompensatingDispatcher>,
    store: &TransactionStore,
    action: &GatewayActionRow,
    compensating_tool: &str,
    compensating_args: &serde_json::Value,
) -> RollbackAction {
    let Some(disp) = dispatcher else {
        return RollbackAction {
            action_id: action.action_id.clone(),
            original_tool: action.tool.clone(),
            compensating_tool: compensating_tool.to_string(),
            success: false,
            error: Some("no compensating dispatcher configured".into()),
        };
    };
    let args_str = compensating_args.to_string();
    match disp.invoke(compensating_tool, &args_str).await {
        Ok(_) => {
            if let Err(e) = store.mark_rolled_back(&action.action_id) {
                tracing::warn!(
                    action_id = %action.action_id,
                    error = %e,
                    "rollback: mark_rolled_back failed; downstream replay won't skip"
                );
            }
            RollbackAction {
                action_id: action.action_id.clone(),
                original_tool: action.tool.clone(),
                compensating_tool: compensating_tool.to_string(),
                success: true,
                error: None,
            }
        }
        Err(e) => RollbackAction {
            action_id: action.action_id.clone(),
            original_tool: action.tool.clone(),
            compensating_tool: compensating_tool.to_string(),
            success: false,
            error: Some(e),
        },
    }
}

// ── Cap registration ─────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub(crate) struct RollbackArgs {
    #[serde(default)]
    pub transaction_id: String,
}

#[derive(Debug, Deserialize, Default)]
pub(crate) struct TransactionGetArgs {
    #[serde(default)]
    pub transaction_id: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct TransactionGetResponse {
    pub transaction_id: String,
    pub actions: Vec<GatewayActionRow>,
    pub count: usize,
}

/// Register the `execution.rollback` and
/// `execution.transaction_get` capabilities. The evidence
/// surface registers its own caps in [`super::evidence`].
pub fn register(
    bridge: &mut DispatchBridge,
    store: Arc<TransactionStore>,
    dispatcher: Option<Arc<dyn CompensatingDispatcher>>,
) {
    {
        let store = store.clone();
        let dispatcher = dispatcher.clone();
        bridge.register(
            "execution.rollback",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let store = store.clone();
                let dispatcher = dispatcher.clone();
                async move { handle_rollback(&store, dispatcher.as_deref(), &ctx).await }
            })),
        );
    }
    {
        let store = store.clone();
        bridge.register(
            "execution.transaction_get",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let store = store.clone();
                async move { handle_transaction_get(&store, &ctx) }
            })),
        );
    }
}

async fn handle_rollback(
    store: &TransactionStore,
    dispatcher: Option<&dyn CompensatingDispatcher>,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let args: RollbackArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("execution.rollback: decode args: {e}")),
    };
    if args.transaction_id.trim().is_empty() {
        return invalid_args("execution.rollback: transaction_id required".into());
    }
    match execute_rollback(store, dispatcher, &args.transaction_id).await {
        Ok(result) => match serde_json::to_vec(&result) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("execution.rollback: encode: {e}")),
        },
        Err(e) => internal(format!("execution.rollback: {e}")),
    }
}

fn handle_transaction_get(store: &TransactionStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: TransactionGetArgs = match serde_json::from_slice(&ctx.args) {
        Ok(a) => a,
        Err(e) => return invalid_args(format!("execution.transaction_get: decode args: {e}")),
    };
    if args.transaction_id.trim().is_empty() {
        return invalid_args("execution.transaction_get: transaction_id required".into());
    }
    let actions = match store.list_for_transaction(&args.transaction_id) {
        Ok(rows) => rows,
        Err(e) => return internal(format!("execution.transaction_get: {e}")),
    };
    let count = actions.len();
    let body = TransactionGetResponse {
        transaction_id: args.transaction_id,
        actions,
        count,
    };
    match serde_json::to_vec(&body) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("execution.transaction_get: encode: {e}")),
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

#[cfg(test)]
mod tests {
    use super::super::transaction_store::build_success_row;
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;

    use super::super::gateway_tier::GatewayDispatchOptions;

    struct StubDispatcher {
        calls: Mutex<Vec<(String, String)>>,
        canned: Mutex<Result<String, String>>,
    }

    impl StubDispatcher {
        fn ok() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                canned: Mutex::new(Ok("ok".into())),
            }
        }
        fn fail(reason: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                canned: Mutex::new(Err(reason.into())),
            }
        }
    }

    #[async_trait]
    impl CompensatingDispatcher for StubDispatcher {
        async fn invoke(&self, tool: &str, args_json: &str) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push((tool.to_string(), args_json.to_string()));
            self.canned.lock().unwrap().clone()
        }
    }

    fn store() -> TransactionStore {
        TransactionStore::open_in_memory().unwrap()
    }

    fn write_tier_a(s: &TransactionStore, tx: &str, comp_tool: &str) -> String {
        let mut row = build_success_row(
            "memory.write",
            r#"{"text":"hi"}"#,
            Some("ok".into()),
            &GatewayDispatchOptions::default()
                .with_transaction_id(tx)
                .auto_compensated(comp_tool, json!({"id": "abc"})),
            10,
            20,
        );
        // Fix the id so tests can refer to it.
        row.action_id = format!("act-A-{comp_tool}");
        s.record(&row).unwrap();
        row.action_id
    }

    fn write_tier_b(s: &TransactionStore, tx: &str, plan: &str) -> String {
        let mut row = build_success_row(
            "email.send",
            r#"{"to":"a"}"#,
            Some("sent".into()),
            &GatewayDispatchOptions::default()
                .with_transaction_id(tx)
                .human_rollback_plan(plan),
            10,
            20,
        );
        row.action_id = format!("act-B-{}", plan.len());
        s.record(&row).unwrap();
        row.action_id
    }

    #[tokio::test]
    async fn rollback_with_tier_a_actions_invokes_compensating_tool() {
        let s = store();
        let id = write_tier_a(&s, "tx-1", "memory.delete");
        let disp = StubDispatcher::ok();
        let result = execute_rollback(&s, Some(&disp), "tx-1").await.unwrap();
        assert_eq!(result.auto_rolled_back.len(), 1);
        let entry = &result.auto_rolled_back[0];
        assert!(entry.success);
        assert_eq!(entry.action_id, id);
        assert_eq!(entry.compensating_tool, "memory.delete");
        // Original action is stamped as rolled-back.
        let row = s.get(&id).unwrap().unwrap();
        assert!(row.rolled_back);
        // Dispatcher saw the compensating tool with the right
        // args JSON.
        let calls = disp.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "memory.delete");
        assert!(calls[0].1.contains("\"id\":\"abc\""));
    }

    #[tokio::test]
    async fn rollback_with_tier_b_action_surfaces_plan_without_invoking() {
        let s = store();
        let id = write_tier_b(&s, "tx-2", "manually retract the email");
        let disp = StubDispatcher::ok();
        let result = execute_rollback(&s, Some(&disp), "tx-2").await.unwrap();
        assert!(result.auto_rolled_back.is_empty());
        assert_eq!(result.human_review_required.len(), 1);
        assert_eq!(result.human_review_required[0].action_id, id);
        assert_eq!(
            result.human_review_required[0].rollback_plan,
            "manually retract the email"
        );
        // Dispatcher was NOT invoked.
        assert!(disp.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rollback_walks_actions_in_reverse_order() {
        let s = store();
        let a1 = write_tier_a(&s, "tx-rev", "memory.delete");
        let a2 = write_tier_a(&s, "tx-rev", "memory.unlink");
        let disp = StubDispatcher::ok();
        let result = execute_rollback(&s, Some(&disp), "tx-rev").await.unwrap();
        // Newest-first: a2 before a1.
        assert_eq!(result.auto_rolled_back[0].action_id, a2);
        assert_eq!(result.auto_rolled_back[1].action_id, a1);
    }

    #[tokio::test]
    async fn rollback_records_failure_when_dispatcher_errors() {
        let s = store();
        let id = write_tier_a(&s, "tx-fail", "memory.delete");
        let disp = StubDispatcher::fail("network down");
        let result = execute_rollback(&s, Some(&disp), "tx-fail").await.unwrap();
        assert_eq!(result.auto_rolled_back.len(), 1);
        assert!(!result.auto_rolled_back[0].success);
        assert_eq!(
            result.auto_rolled_back[0].error.as_deref(),
            Some("network down")
        );
        // The flag is NOT set since the compensating call failed.
        let row = s.get(&id).unwrap().unwrap();
        assert!(!row.rolled_back);
    }

    #[tokio::test]
    async fn rollback_with_no_dispatcher_marks_each_tier_a_as_failed() {
        let s = store();
        write_tier_a(&s, "tx-nodisp", "memory.delete");
        let result = execute_rollback(&s, None, "tx-nodisp").await.unwrap();
        assert_eq!(result.auto_rolled_back.len(), 1);
        assert!(!result.auto_rolled_back[0].success);
        assert_eq!(
            result.auto_rolled_back[0].error.as_deref(),
            Some("no compensating dispatcher configured")
        );
    }

    #[tokio::test]
    async fn rollback_skips_already_rolled_back_actions() {
        let s = store();
        let id = write_tier_a(&s, "tx-idem", "memory.delete");
        s.mark_rolled_back(&id).unwrap();
        let disp = StubDispatcher::ok();
        let result = execute_rollback(&s, Some(&disp), "tx-idem").await.unwrap();
        assert!(result.auto_rolled_back.is_empty());
        assert!(disp.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rollback_skips_failed_or_dry_run_actions() {
        let s = store();
        // dry_run row
        let mut dry = build_success_row(
            "tool.x",
            "{}",
            Some("preview".into()),
            &GatewayDispatchOptions::default()
                .with_transaction_id("tx-dry")
                .auto_compensated("tool.x.undo", json!({})),
            1,
            2,
        );
        dry.action_id = "dry-1".into();
        dry.dry_run = true;
        s.record(&dry).unwrap();
        // failed row
        let mut bad = dry.clone();
        bad.action_id = "bad-1".into();
        bad.dry_run = false;
        bad.success = false;
        bad.error = Some("h e l l o".into());
        s.record(&bad).unwrap();
        let disp = StubDispatcher::ok();
        let result = execute_rollback(&s, Some(&disp), "tx-dry").await.unwrap();
        assert!(result.auto_rolled_back.is_empty());
        assert!(result.human_review_required.is_empty());
    }
}
