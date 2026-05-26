//! Central tool dispatcher.
//!
//! Every tool call routes through this struct so the same
//! pre/post checks fire regardless of which capability is
//! being invoked:
//!
//! 1. **Access broker** — the agent must have permission to
//!    call the named capability.
//! 2. **Secret resolution** — `{{secret:name}}` placeholders
//!    in the args get rewritten to the live value.
//! 3. **Handler dispatch** — the operator-supplied async fn
//!    runs with the resolved args.
//! 4. **Gateway record** — the action goes into the
//!    transaction summary regardless of outcome.
//!
//! The dispatcher is the choke-point: future security
//! additions (cost cap, output guard, audit ring) attach
//! here so every tool call gets them for free.

use std::sync::{Arc, Mutex};

use super::super::execution::broker::{AccessDecision, AgentAccessBroker};
use super::super::execution::gateway::{ActionGateway, GatewayAction};
use super::super::execution::secrets::{SecretError, SecretStore};
use super::contracts::ToolContract;
use super::output_guard::ToolOutputGuard;

/// Errors the dispatcher surfaces. Mirrors the shape of the
/// `AccessDecision` variants so callers can pattern-match
/// without re-translating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchError {
    AccessDenied(String),
    RateLimited {
        retry_after_secs: u64,
    },
    SecretMissing(String),
    HandlerFailed(String),
    /// Args failed the contract's input-schema validation.
    /// Carries the list of human-readable validation errors.
    InvalidInput(Vec<String>),
    /// Handler reply failed the contract's output-schema
    /// validation. The handler ran (so the side effect may
    /// have happened) — the dispatcher logs + surfaces the
    /// reason so callers can decide whether to retry.
    InvalidOutput(Vec<String>),
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AccessDenied(r) => write!(f, "access denied: {r}"),
            Self::RateLimited { retry_after_secs } => {
                write!(f, "rate limited; retry after {retry_after_secs}s")
            }
            Self::SecretMissing(name) => write!(f, "secret '{name}' not found"),
            Self::HandlerFailed(c) => write!(f, "handler failed: {c}"),
            Self::InvalidInput(errs) => write!(f, "invalid input: {}", errs.join("; ")),
            Self::InvalidOutput(errs) => write!(f, "invalid output: {}", errs.join("; ")),
        }
    }
}

impl std::error::Error for DispatchError {}

/// Central dispatcher. Cheap to clone (three Arcs).
#[derive(Clone)]
pub struct ToolDispatcher {
    secret_store: Arc<SecretStore>,
    broker: Arc<AgentAccessBroker>,
    gateway: Arc<Mutex<ActionGateway>>,
}

impl ToolDispatcher {
    pub fn new(secret_store: Arc<SecretStore>, broker: Arc<AgentAccessBroker>) -> Self {
        Self {
            secret_store,
            broker,
            gateway: Arc::new(Mutex::new(ActionGateway::new())),
        }
    }

    /// Dispatch a single tool call. The `handler` closure
    /// receives the secret-resolved args and returns the
    /// tool's reply text. The dispatcher records the
    /// outcome to the action gateway whether the handler
    /// succeeds or fails.
    pub async fn dispatch<F, Fut>(
        &self,
        agent: &str,
        tool: &str,
        args: &str,
        reversible: bool,
        rollback_hint: Option<String>,
        handler: F,
    ) -> Result<String, DispatchError>
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Result<String, String>>,
    {
        // 1. Access broker.
        match self.broker.check(agent, tool) {
            AccessDecision::Allow => {}
            AccessDecision::Deny { reason } => return Err(DispatchError::AccessDenied(reason)),
            AccessDecision::RateLimited { retry_after_secs } => {
                return Err(DispatchError::RateLimited { retry_after_secs });
            }
        }
        // 2. Secret resolution.
        let resolved_args = match self.secret_store.resolve(args) {
            Ok(s) => s,
            Err(SecretError::Missing(name, _hint)) => {
                let action = GatewayAction::new(tool, args, reversible);
                let action = match &rollback_hint {
                    Some(h) => action.with_rollback_hint(h.clone()),
                    None => action,
                };
                self.gateway.lock().unwrap().record_failed(action);
                return Err(DispatchError::SecretMissing(name));
            }
        };
        // 3. Handler dispatch.
        let result = handler(resolved_args.clone()).await;
        // 4. Gateway record.
        let mut action = GatewayAction::new(tool, &resolved_args, reversible);
        if let Some(h) = &rollback_hint {
            action = action.with_rollback_hint(h.clone());
        }
        match result {
            Ok(output) => {
                // Output guard. Runs before the gateway record
                // so a poisoned reply lands as `failed` (the
                // operator still sees the attempt, but the
                // upstream sees `HandlerFailed` rather than a
                // contaminated success). Truncation alone is
                // permitted to pass through — long replies are
                // common; injection is what we have to stop.
                let guard = ToolOutputGuard::inspect(&output);
                if guard.injection_detected {
                    let reason = guard
                        .reason
                        .clone()
                        .unwrap_or_else(|| "tool output flagged by guard".to_string());
                    tracing::warn!(
                        agent,
                        tool,
                        reason = %reason,
                        "tool dispatch: output guard rejected reply"
                    );
                    self.gateway.lock().unwrap().record_failed(action);
                    return Err(DispatchError::HandlerFailed(reason));
                }
                let safe_output = guard.output;
                if guard.truncated {
                    tracing::warn!(
                        agent,
                        tool,
                        "tool dispatch: output truncated by guard (>50k chars)"
                    );
                }
                let recorded = action.with_result(safe_output.clone());
                self.gateway.lock().unwrap().record_completed(recorded);
                // Successful dispatches feed the broker's
                // rate limiter so the agent's window
                // includes this call.
                self.broker.record_call(agent);
                Ok(safe_output)
            }
            Err(reason) => {
                self.gateway.lock().unwrap().record_failed(action);
                Err(DispatchError::HandlerFailed(reason))
            }
        }
    }

    /// Schema-validated dispatch. Wraps [`Self::dispatch`]
    /// with JSON parsing + input/output validation against
    /// the supplied [`ToolContract`].
    ///
    /// Flow:
    /// 1. Parse `args` as JSON. Non-JSON args → `InvalidInput`.
    /// 2. Validate against `contract.input_schema`.
    /// 3. Re-serialise the validated input as a string and
    ///    pass it to `dispatch` (which runs the broker + secret
    ///    + handler + gateway pipeline).
    /// 4. Parse the handler reply as JSON.
    /// 5. Validate against `contract.output_schema`.
    /// 6. Return the original reply text on success.
    pub async fn dispatch_with_contract<F, Fut>(
        &self,
        agent: &str,
        contract: &ToolContract,
        args: &str,
        reversible: bool,
        rollback_hint: Option<String>,
        handler: F,
    ) -> Result<String, DispatchError>
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Result<String, String>>,
    {
        let input_value: serde_json::Value = match serde_json::from_str(args) {
            Ok(v) => v,
            Err(e) => {
                return Err(DispatchError::InvalidInput(vec![format!(
                    "args are not valid JSON: {e}"
                )]));
            }
        };
        if let Err(errs) = contract.validate_input(&input_value) {
            return Err(DispatchError::InvalidInput(errs));
        }
        let resolved_args = match serde_json::to_string(&input_value) {
            Ok(s) => s,
            Err(e) => {
                return Err(DispatchError::InvalidInput(vec![format!(
                    "re-serialise: {e}"
                )]));
            }
        };
        let reply = self
            .dispatch(
                agent,
                &contract.tool_name,
                &resolved_args,
                reversible,
                rollback_hint,
                handler,
            )
            .await?;
        let output_value: serde_json::Value = match serde_json::from_str(&reply) {
            Ok(v) => v,
            Err(e) => {
                return Err(DispatchError::InvalidOutput(vec![format!(
                    "handler reply is not valid JSON: {e}"
                )]));
            }
        };
        if let Err(errs) = contract.validate_output(&output_value) {
            return Err(DispatchError::InvalidOutput(errs));
        }
        Ok(reply)
    }

    /// Render the current gateway state. Used by the
    /// evidence-capture path to emit one chronicle entry per
    /// `ai.chat` turn that spawned tool calls.
    pub fn gateway_snapshot(&self) -> String {
        self.gateway.lock().unwrap().transaction_summary()
    }

    /// `true` if any irreversible action completed before a
    /// failure occurred. The caller surfaces the rollback
    /// notification when this fires.
    pub fn needs_rollback_notification(&self) -> bool {
        self.gateway.lock().unwrap().needs_rollback_notification()
    }

    pub fn rollback_notification(&self) -> String {
        self.gateway.lock().unwrap().rollback_notification()
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::execution::broker::AccessPolicy;
    use super::*;
    use std::collections::BTreeMap;

    fn store_with(pairs: &[(&str, &str)]) -> Arc<SecretStore> {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), (*v).to_string());
        }
        Arc::new(SecretStore::from_map(m))
    }

    fn broker_with(policies: Vec<AccessPolicy>) -> Arc<AgentAccessBroker> {
        Arc::new(AgentAccessBroker::new(policies))
    }

    fn empty_broker() -> Arc<AgentAccessBroker> {
        Arc::new(AgentAccessBroker::empty())
    }

    fn policy(agent: &str, deny: &[&str]) -> AccessPolicy {
        AccessPolicy {
            agent: agent.to_string(),
            allowed_capabilities: Vec::new(),
            denied_capabilities: deny.iter().map(|s| s.to_string()).collect(),
            max_calls_per_minute: 60,
            max_cost_cents_per_hour: 500,
        }
    }

    #[tokio::test]
    async fn dispatch_denied_by_broker_returns_access_denied() {
        let store = store_with(&[]);
        let broker = broker_with(vec![policy("alice", &["tool.terminal"])]);
        let dispatcher = ToolDispatcher::new(store, broker);
        let err = dispatcher
            .dispatch("alice", "tool.terminal", "ls", true, None, |_args| async {
                Ok("never called".into())
            })
            .await
            .unwrap_err();
        match err {
            DispatchError::AccessDenied(reason) => {
                assert!(reason.contains("deny list"));
            }
            other => panic!("expected AccessDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_resolves_secrets_before_calling_handler() {
        let store = store_with(&[("github_token", "ghp_secretvalue")]);
        let broker = empty_broker();
        let dispatcher = ToolDispatcher::new(store, broker);
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();
        let out = dispatcher
            .dispatch(
                "alice",
                "web.fetch",
                "Authorization: Bearer {{secret:github_token}}",
                true,
                None,
                |args| async move {
                    *captured_clone.lock().unwrap() = Some(args.clone());
                    Ok(format!("called with {args}"))
                },
            )
            .await
            .unwrap();
        let seen = captured.lock().unwrap().clone().unwrap();
        assert_eq!(seen, "Authorization: Bearer ghp_secretvalue");
        assert!(out.contains("ghp_secretvalue"));
    }

    #[tokio::test]
    async fn dispatch_records_completed_to_gateway() {
        let store = store_with(&[]);
        let broker = empty_broker();
        let dispatcher = ToolDispatcher::new(store, broker);
        dispatcher
            .dispatch(
                "alice",
                "web.fetch",
                "https://example.com",
                true,
                None,
                |_| async { Ok("body".into()) },
            )
            .await
            .unwrap();
        let snap = dispatcher.gateway_snapshot();
        assert!(snap.contains("completed=1 failed=0"));
        assert!(snap.contains("OK   [rev] web.fetch"));
    }

    #[tokio::test]
    async fn dispatch_records_failed_to_gateway_with_rollback_hint() {
        let store = store_with(&[]);
        let broker = empty_broker();
        let dispatcher = ToolDispatcher::new(store, broker);
        let err = dispatcher
            .dispatch(
                "alice",
                "email.send",
                "to=ops",
                false,
                Some("manually retract the email".into()),
                |_| async { Err("smtp 500".into()) },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DispatchError::HandlerFailed(_)));
        let snap = dispatcher.gateway_snapshot();
        assert!(snap.contains("completed=0 failed=1"));
        assert!(snap.contains("FAIL [irrev] email.send"));
    }

    #[tokio::test]
    async fn dispatch_returns_secret_missing_and_records_failure() {
        let store = store_with(&[]);
        let broker = empty_broker();
        let dispatcher = ToolDispatcher::new(store, broker);
        let err = dispatcher
            .dispatch(
                "alice",
                "web.fetch",
                "Authorization: Bearer {{secret:missing_token}}",
                true,
                None,
                |_| async { Ok("never called".into()) },
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::SecretMissing(name) => assert_eq!(name, "missing_token"),
            other => panic!("expected SecretMissing, got {other:?}"),
        }
        let snap = dispatcher.gateway_snapshot();
        assert!(snap.contains("completed=0 failed=1"));
    }

    #[tokio::test]
    async fn gateway_snapshot_after_multiple_dispatches_lists_all_actions() {
        let store = store_with(&[]);
        let broker = empty_broker();
        let dispatcher = ToolDispatcher::new(store, broker);
        for i in 0..3 {
            dispatcher
                .dispatch(
                    "alice",
                    "web.fetch",
                    &format!("https://example.com/{i}"),
                    true,
                    None,
                    |_| async { Ok("body".into()) },
                )
                .await
                .unwrap();
        }
        let snap = dispatcher.gateway_snapshot();
        assert!(snap.contains("completed=3"));
    }

    #[tokio::test]
    async fn rate_limited_dispatch_returns_retry_after() {
        let store = store_with(&[]);
        let broker = broker_with(vec![AccessPolicy {
            agent: "alice".to_string(),
            allowed_capabilities: Vec::new(),
            denied_capabilities: Vec::new(),
            max_calls_per_minute: 1,
            max_cost_cents_per_hour: 500,
        }]);
        let dispatcher = ToolDispatcher::new(store, broker);
        // First call burns the rate-limit budget.
        dispatcher
            .dispatch(
                "alice",
                "web.fetch",
                "https://example.com",
                true,
                None,
                |_| async { Ok("body".into()) },
            )
            .await
            .unwrap();
        // Second call should hit the cap.
        let err = dispatcher
            .dispatch(
                "alice",
                "web.fetch",
                "https://example.com",
                true,
                None,
                |_| async { Ok("body".into()) },
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::RateLimited { retry_after_secs } => {
                assert!(retry_after_secs <= 60);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_with_contract_validates_input_and_output() {
        use super::super::contracts::fs_write_contract;
        let store = store_with(&[]);
        let broker = empty_broker();
        let dispatcher = ToolDispatcher::new(store, broker);
        let contract = fs_write_contract();
        // Happy path: valid JSON, valid handler reply.
        let out = dispatcher
            .dispatch_with_contract(
                "alice",
                &contract,
                r#"{"path":"/tmp/x","content":"hi"}"#,
                true,
                None,
                |_| async { Ok(r#"{"ok":"wrote 2 bytes"}"#.into()) },
            )
            .await
            .unwrap();
        assert!(out.contains("wrote 2 bytes"));
        // Invalid input: missing `content`.
        let err = dispatcher
            .dispatch_with_contract(
                "alice",
                &contract,
                r#"{"path":"/tmp/x"}"#,
                true,
                None,
                |_| async { Ok("unreached".into()) },
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::InvalidInput(errs) => {
                assert!(errs.iter().any(|e| e.contains("content")));
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
        // Invalid output: handler returns a reply missing the
        // `ok` field.
        let err = dispatcher
            .dispatch_with_contract(
                "alice",
                &contract,
                r#"{"path":"/tmp/x","content":"hi"}"#,
                true,
                None,
                |_| async { Ok(r#"{"something_else":1}"#.into()) },
            )
            .await
            .unwrap_err();
        match err {
            DispatchError::InvalidOutput(errs) => {
                assert!(errs.iter().any(|e| e.contains("ok")));
            }
            other => panic!("expected InvalidOutput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rollback_notification_signals_when_irreversible_completed_before_failure() {
        let store = store_with(&[]);
        let broker = empty_broker();
        let dispatcher = ToolDispatcher::new(store, broker);
        // 1. Irreversible action completes.
        dispatcher
            .dispatch(
                "alice",
                "email.send",
                "to=ops",
                false,
                Some("manually retract".into()),
                |_| async { Ok("sent".into()) },
            )
            .await
            .unwrap();
        // 2. Reversible action fails.
        let _ = dispatcher
            .dispatch("alice", "db.commit", "x", true, None, |_| async {
                Err("rollback".into())
            })
            .await;
        assert!(dispatcher.needs_rollback_notification());
        let notice = dispatcher.rollback_notification();
        assert!(notice.contains("ROLLBACK NEEDED"));
        assert!(notice.contains("email.send"));
        assert!(notice.contains("manually retract"));
    }
}
