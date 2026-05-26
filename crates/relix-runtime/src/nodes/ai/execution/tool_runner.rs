//! Walks `ExecutionPlan` ToolCall steps through the central
//! [`ToolDispatcher`].
//!
//! When the planner emits a `<plan>` block with `tool: ...`
//! entries, each entry lands as a [`PlanStep::ToolCall`]. This
//! runner takes the parsed plan + the per-controller
//! dispatcher + the calling agent's name and routes every
//! ToolCall through the dispatcher's pre/post pipeline:
//!
//! 1. **Access broker** check.
//! 2. **Secret** resolution of `{{secret:name}}` placeholders.
//! 3. **Handler** dispatch — the AI controller's admission
//!    closure (the actual mesh hop to the tool node happens on
//!    the tool-flow path; from the AI controller's vantage,
//!    the dispatcher's job is to admit + record).
//! 4. **Output guard** + **gateway** recording.
//!
//! When any check fails, the runner records a
//! [`StepResult::Err`] with a JSON-shaped reason so the chat
//! response carries a structured error instead of silently
//! dropping the call.

use super::executor::StepResult;
use super::planner::{ExecutionPlan, PlanStep};
use crate::nodes::tool::dispatcher::{DispatchError, ToolDispatcher};

/// Walk every [`PlanStep::ToolCall`] in `plan` through
/// `dispatcher`. Returns one [`StepResult`] per ToolCall in
/// plan order. Non-ToolCall steps are skipped.
pub async fn dispatch_planner_tool_calls(
    dispatcher: &ToolDispatcher,
    agent: &str,
    plan: &ExecutionPlan,
) -> Vec<StepResult> {
    let mut results = Vec::new();
    for step in &plan.steps {
        if let PlanStep::ToolCall { tool, args } = step {
            let reversible = !is_irreversible_tool(tool);
            let tool_label = tool.clone();
            let outcome = dispatcher
                .dispatch(
                    agent,
                    tool,
                    args,
                    reversible,
                    None,
                    move |resolved_args| async move {
                        // The AI controller owns admission. The
                        // actual mesh hop to the tool node sits
                        // on the tool-flow path. The handler
                        // here records the admitted call so the
                        // gateway has a recordable result.
                        Ok(format!(
                            "admitted: tool={tool_label} args_len={}",
                            resolved_args.len()
                        ))
                    },
                )
                .await;
            results.push(match outcome {
                Ok(out) => StepResult::Ok { output: out },
                Err(err) => StepResult::Err {
                    reason: structured_dispatch_error(&err),
                },
            });
        }
    }
    results
}

/// Render a [`DispatchError`] as a JSON object so chat clients
/// can parse it deterministically. Mirrors the variant shape
/// of `DispatchError` so a future schema break here would fail
/// the unit tests in this module.
pub fn structured_dispatch_error(err: &DispatchError) -> String {
    match err {
        DispatchError::AccessDenied(reason) => serde_json::json!({
            "kind": "access_denied",
            "reason": reason,
        })
        .to_string(),
        DispatchError::RateLimited { retry_after_secs } => serde_json::json!({
            "kind": "rate_limited",
            "retry_after_secs": retry_after_secs,
        })
        .to_string(),
        DispatchError::SecretMissing(name) => serde_json::json!({
            "kind": "secret_missing",
            "secret": name,
        })
        .to_string(),
        DispatchError::HandlerFailed(cause) => serde_json::json!({
            "kind": "handler_failed",
            "cause": cause,
        })
        .to_string(),
        DispatchError::InvalidInput(errs) => serde_json::json!({
            "kind": "invalid_input",
            "errors": errs,
        })
        .to_string(),
        DispatchError::InvalidOutput(errs) => serde_json::json!({
            "kind": "invalid_output",
            "errors": errs,
        })
        .to_string(),
    }
}

/// Mirror of `planner::irreversible_tool`. Kept private +
/// duplicated so the runner doesn't need to expose the
/// planner's heuristic via a fresh accessor; the keyword list
/// is short enough that drift between the two will surface in
/// review.
fn is_irreversible_tool(tool: &str) -> bool {
    let lower = tool.to_ascii_lowercase();
    for kw in [
        "write",
        "delete",
        "remove",
        "send",
        "post",
        "drop",
        "destroy",
        "publish",
        "overwrite",
    ] {
        if lower.contains(kw) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::super::planner::{ExecutionPlan, PlanStep, Reversibility};
    use super::*;
    use crate::nodes::execution::broker::{AccessPolicy, AgentAccessBroker};
    use crate::nodes::execution::secrets::SecretStore;

    fn empty_secrets() -> Arc<SecretStore> {
        Arc::new(SecretStore::from_map(BTreeMap::new()))
    }

    fn secrets_with(pairs: &[(&str, &str)]) -> Arc<SecretStore> {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), (*v).to_string());
        }
        Arc::new(SecretStore::from_map(m))
    }

    fn plan_with_tool(tool: &str, args: &str) -> ExecutionPlan {
        ExecutionPlan {
            steps: vec![PlanStep::ToolCall {
                tool: tool.into(),
                args: args.into(),
            }],
            estimated_cost_cents: 0,
            requires_approval: false,
            reversibility: Reversibility::Reversible,
        }
    }

    #[tokio::test]
    async fn tool_call_passing_broker_check_executes_and_is_recorded_in_gateway() {
        let dispatcher = ToolDispatcher::new(empty_secrets(), Arc::new(AgentAccessBroker::empty()));
        let plan = plan_with_tool("web.fetch", "https://example.com");
        let results = dispatch_planner_tool_calls(&dispatcher, "alice", &plan).await;
        assert_eq!(results.len(), 1);
        match &results[0] {
            StepResult::Ok { output } => {
                assert!(output.contains("admitted"));
                assert!(output.contains("web.fetch"));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        let snap = dispatcher.gateway_snapshot();
        assert!(snap.contains("completed=1 failed=0"), "snap={snap}");
        assert!(snap.contains("web.fetch"));
    }

    #[tokio::test]
    async fn tool_call_failing_broker_check_is_not_executed_returns_structured_error() {
        let broker = Arc::new(AgentAccessBroker::new(vec![AccessPolicy {
            agent: "alice".into(),
            allowed_capabilities: Vec::new(),
            denied_capabilities: vec!["tool.terminal".into()],
            max_calls_per_minute: 60,
            max_cost_cents_per_hour: 500,
        }]));
        let dispatcher = ToolDispatcher::new(empty_secrets(), broker);
        let plan = plan_with_tool("tool.terminal", "rm -rf /");
        let results = dispatch_planner_tool_calls(&dispatcher, "alice", &plan).await;
        assert_eq!(results.len(), 1);
        match &results[0] {
            StepResult::Err { reason } => {
                let v: serde_json::Value =
                    serde_json::from_str(reason).expect("dispatch error is JSON");
                assert_eq!(v["kind"], "access_denied");
                let r = v["reason"].as_str().expect("reason is string");
                assert!(r.contains("deny list"), "reason={r}");
                assert!(r.contains("tool.terminal"), "reason={r}");
            }
            other => panic!("expected Err, got {other:?}"),
        }
        // Broker-denied calls never reach the gateway; the
        // tool did not execute.
        let snap = dispatcher.gateway_snapshot();
        assert!(snap.contains("completed=0 failed=0"), "snap={snap}");
    }

    #[tokio::test]
    async fn secret_placeholders_in_tool_args_are_resolved_before_handler_runs() {
        let dispatcher = ToolDispatcher::new(
            secrets_with(&[("github_token", "ghp_real")]),
            Arc::new(AgentAccessBroker::empty()),
        );
        let plan = plan_with_tool("web.fetch", "Authorization: Bearer {{secret:github_token}}");
        let results = dispatch_planner_tool_calls(&dispatcher, "alice", &plan).await;
        // Resolved args = `Authorization: Bearer ghp_real` = 30 chars.
        match &results[0] {
            StepResult::Ok { output } => {
                assert!(
                    output.contains("args_len=30"),
                    "resolved args did not flow into handler; output={output}"
                );
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_secret_returns_structured_secret_missing_error() {
        let dispatcher = ToolDispatcher::new(empty_secrets(), Arc::new(AgentAccessBroker::empty()));
        let plan = plan_with_tool(
            "web.fetch",
            "Authorization: Bearer {{secret:missing_token}}",
        );
        let results = dispatch_planner_tool_calls(&dispatcher, "alice", &plan).await;
        match &results[0] {
            StepResult::Err { reason } => {
                let v: serde_json::Value =
                    serde_json::from_str(reason).expect("dispatch error is JSON");
                assert_eq!(v["kind"], "secret_missing");
                assert_eq!(v["secret"], "missing_token");
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_tool_call_steps_are_skipped() {
        let dispatcher = ToolDispatcher::new(empty_secrets(), Arc::new(AgentAccessBroker::empty()));
        let plan = ExecutionPlan {
            steps: vec![
                PlanStep::ModelCall {
                    prompt: "hi".into(),
                    model: "m".into(),
                },
                PlanStep::MemoryRead { query: "x".into() },
                PlanStep::HumanApproval {
                    reason: "ok?".into(),
                },
            ],
            estimated_cost_cents: 0,
            requires_approval: false,
            reversibility: Reversibility::Reversible,
        };
        let results = dispatch_planner_tool_calls(&dispatcher, "alice", &plan).await;
        // No ToolCall steps → no results.
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn irreversible_tool_call_is_recorded_as_irreversible_in_gateway() {
        let dispatcher = ToolDispatcher::new(empty_secrets(), Arc::new(AgentAccessBroker::empty()));
        let plan = plan_with_tool("email.send", "to=ops");
        let _ = dispatch_planner_tool_calls(&dispatcher, "alice", &plan).await;
        let snap = dispatcher.gateway_snapshot();
        assert!(snap.contains("IRREVERSIBLE"), "snap={snap}");
        assert!(snap.contains("email.send"));
    }
}
