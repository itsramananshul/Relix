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

use std::sync::Arc;

use super::executor::StepResult;
use super::planner::{ExecutionPlan, PlanStep};
use crate::nodes::tool::dispatcher::{DispatchError, ToolDispatcher};

/// Outbound mesh shim used by the tool runner to actually
/// execute a planner-emitted ToolCall after the dispatcher
/// admits it.
///
/// Implementations dial the configured tool peer (typically
/// alias `"tool"`) over libp2p and return the responder's
/// UTF-8 reply, or an operator-facing error string on
/// transport / responder failure. The trait keeps the AI
/// runtime decoupled from `MeshClient`: tests inject a stub,
/// production wires the real client.
#[async_trait::async_trait]
pub trait ToolMeshDispatcher: Send + Sync {
    /// Call `tool` with `resolved_args` and return the responder's
    /// UTF-8 reply. Errors surface as the human-readable string
    /// the [`DispatchError::HandlerFailed`] variant carries.
    async fn call(&self, tool: &str, resolved_args: &str) -> Result<String, String>;
}

/// Walk every [`PlanStep::ToolCall`] in `plan` through
/// `dispatcher`. Returns one [`StepResult`] per ToolCall in
/// plan order. Non-ToolCall steps are skipped.
///
/// When `mesh` is `Some(...)` the dispatcher's handler closure
/// dials the tool peer through the supplied
/// [`ToolMeshDispatcher`] AFTER the broker + secret-resolve
/// pre-checks. When `None`, the closure records an
/// `admitted: ...` marker — useful while no outbound transport
/// is wired but operator-honest about the fact that the call
/// did not execute.
pub async fn dispatch_planner_tool_calls(
    dispatcher: &ToolDispatcher,
    agent: &str,
    plan: &ExecutionPlan,
    mesh: Option<Arc<dyn ToolMeshDispatcher>>,
) -> Vec<StepResult> {
    let mut results = Vec::new();
    for step in &plan.steps {
        if let PlanStep::ToolCall { tool, args } = step {
            let reversible = !is_irreversible_tool(tool);
            let tool_label = tool.clone();
            let mesh_clone = mesh.clone();
            let outcome = dispatcher
                .dispatch(
                    agent,
                    tool,
                    args,
                    reversible,
                    None,
                    move |resolved_args| async move {
                        match mesh_clone.as_ref() {
                            Some(m) => {
                                // Real outbound dispatch. The
                                // dispatcher already validated
                                // identity / broker / secrets;
                                // any responder-side error
                                // surfaces back as
                                // `HandlerFailed` and lands as
                                // a structured error in the
                                // chat response trailer.
                                m.call(&tool_label, &resolved_args).await
                            }
                            None => {
                                // No mesh wired — keep the
                                // admit-only behaviour so the
                                // dispatcher pipeline still
                                // runs broker + secret + output
                                // guard + gateway, and the
                                // operator sees an honest
                                // "admitted but not executed"
                                // gateway entry.
                                Ok(format!(
                                    "admitted: tool={tool_label} args_len={}",
                                    resolved_args.len()
                                ))
                            }
                        }
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
        let results = dispatch_planner_tool_calls(&dispatcher, "alice", &plan, None).await;
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
        let results = dispatch_planner_tool_calls(&dispatcher, "alice", &plan, None).await;
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
        let results = dispatch_planner_tool_calls(&dispatcher, "alice", &plan, None).await;
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
        let results = dispatch_planner_tool_calls(&dispatcher, "alice", &plan, None).await;
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
        let results = dispatch_planner_tool_calls(&dispatcher, "alice", &plan, None).await;
        // No ToolCall steps → no results.
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn irreversible_tool_call_is_recorded_as_irreversible_in_gateway() {
        let dispatcher = ToolDispatcher::new(empty_secrets(), Arc::new(AgentAccessBroker::empty()));
        let plan = plan_with_tool("email.send", "to=ops");
        let _ = dispatch_planner_tool_calls(&dispatcher, "alice", &plan, None).await;
        let snap = dispatcher.gateway_snapshot();
        assert!(snap.contains("IRREVERSIBLE"), "snap={snap}");
        assert!(snap.contains("email.send"));
    }

    /// Stub mesh dispatcher that records every call it receives
    /// and returns a fixed reply. Lets the test confirm the
    /// runner actually invokes the mesh hop after the
    /// dispatcher admits the call.
    struct StubMesh {
        calls: std::sync::Mutex<Vec<(String, String)>>,
        reply: String,
    }

    #[async_trait::async_trait]
    impl ToolMeshDispatcher for StubMesh {
        async fn call(&self, tool: &str, args: &str) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push((tool.to_string(), args.to_string()));
            Ok(self.reply.clone())
        }
    }

    #[tokio::test]
    async fn runner_calls_mesh_dispatcher_with_resolved_args_when_provided() {
        let dispatcher = ToolDispatcher::new(
            secrets_with(&[("github_token", "ghp_real")]),
            Arc::new(AgentAccessBroker::empty()),
        );
        let plan = plan_with_tool("web.fetch", "Authorization: Bearer {{secret:github_token}}");
        let mesh = Arc::new(StubMesh {
            calls: std::sync::Mutex::new(Vec::new()),
            reply: "https://example.com response body".into(),
        });
        let results =
            dispatch_planner_tool_calls(&dispatcher, "alice", &plan, Some(mesh.clone())).await;
        // The runner forwards to the mesh; the responder's
        // reply lands as StepResult::Ok.
        match &results[0] {
            StepResult::Ok { output } => {
                assert_eq!(output, "https://example.com response body");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        // Confirm the mesh dispatcher actually got called AND
        // with the secret-resolved args (not the placeholder).
        let calls = mesh.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "web.fetch");
        assert_eq!(calls[0].1, "Authorization: Bearer ghp_real");
    }

    struct FailingMesh;

    #[async_trait::async_trait]
    impl ToolMeshDispatcher for FailingMesh {
        async fn call(&self, _tool: &str, _args: &str) -> Result<String, String> {
            Err("simulated transport drop".to_string())
        }
    }

    #[tokio::test]
    async fn runner_surfaces_mesh_error_as_structured_handler_failed() {
        let dispatcher = ToolDispatcher::new(empty_secrets(), Arc::new(AgentAccessBroker::empty()));
        let plan = plan_with_tool("web.fetch", "https://example.com");
        let results =
            dispatch_planner_tool_calls(&dispatcher, "alice", &plan, Some(Arc::new(FailingMesh)))
                .await;
        match &results[0] {
            StepResult::Err { reason } => {
                let v: serde_json::Value = serde_json::from_str(reason).expect("structured error");
                assert_eq!(v["kind"], "handler_failed");
                assert!(v["cause"].as_str().unwrap().contains("transport drop"));
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }
}
