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
use super::super::execution::gateway_tier::{DryRunPreview, GatewayDispatchOptions, GatewayTier};
use super::super::execution::secrets::{SecretError, SecretStore};
use super::super::execution::transaction_store::{
    GatewayActionRow, TransactionStore, build_failure_row, build_success_row,
};
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

/// Central dispatcher. Cheap to clone (Arcs inside).
#[derive(Clone)]
pub struct ToolDispatcher {
    secret_store: Arc<SecretStore>,
    broker: Arc<AgentAccessBroker>,
    gateway: Arc<Mutex<ActionGateway>>,
    /// GAP 11: optional persistent transaction store. When
    /// `Some`, every `dispatch_with_options` call also writes
    /// a row to `gateway_actions` so `execution.rollback` can
    /// replay it. `None` keeps the in-memory `ActionGateway`
    /// behaviour for backwards-compat callers.
    transaction_store: Option<Arc<TransactionStore>>,
    /// GAP 11: optional evidence sink. When `Some`, the
    /// dispatcher captures one structured evidence record per
    /// `dispatch_with_options` call.
    evidence_sink: Option<Arc<dyn EvidenceCaptureSink>>,
    /// GAP 11: Tier C — list of tools that are never permitted.
    /// Populated from `[execution.gateway] blocked_tools`. An
    /// empty list means no static blocks; per-call Tier C
    /// classification via `GatewayDispatchOptions::blocked()`
    /// still applies.
    blocked_tools: Arc<Vec<String>>,
    /// GAP 11: when `true`, every call goes through the
    /// dry-run preview path regardless of the per-call
    /// `dry_run` flag. Operators flip this on for
    /// pre-production runs.
    global_dry_run: bool,
}

/// Trait the dispatcher uses to hand evidence records to a
/// store. Decoupled from the concrete `EvidenceStore` in
/// `nodes/execution/evidence.rs` so the dispatcher compiles
/// without the evidence wiring (and tests can stub it).
pub trait EvidenceCaptureSink: Send + Sync {
    /// Called once per `dispatch_with_options` call AFTER the
    /// action row has been built. Implementations capture
    /// state_before/state_after, redact args, compute diffs,
    /// and persist the evidence row.
    fn capture(&self, ctx: EvidenceCaptureCtx<'_>);
}

/// Per-call context handed to [`EvidenceCaptureSink::capture`].
pub struct EvidenceCaptureCtx<'a> {
    pub action: &'a GatewayActionRow,
    pub agent: &'a str,
    pub tool: &'a str,
    pub args: &'a str,
    pub result: Option<&'a str>,
    pub error: Option<&'a str>,
    pub started_at_ms: i64,
    pub completed_at_ms: i64,
}

impl ToolDispatcher {
    pub fn new(secret_store: Arc<SecretStore>, broker: Arc<AgentAccessBroker>) -> Self {
        Self {
            secret_store,
            broker,
            gateway: Arc::new(Mutex::new(ActionGateway::new())),
            transaction_store: None,
            evidence_sink: None,
            blocked_tools: Arc::new(Vec::new()),
            global_dry_run: false,
        }
    }

    /// GAP 11: install the persistent transaction store. The
    /// dispatcher records every `dispatch_with_options` call
    /// into this store so `execution.rollback` can reach back.
    pub fn with_transaction_store(mut self, store: Arc<TransactionStore>) -> Self {
        self.transaction_store = Some(store);
        self
    }

    /// GAP 11: install the static Tier C list. Tools on this
    /// list are rejected with `AccessDenied` before any
    /// handler dispatch.
    pub fn with_blocked_tools(mut self, blocked: Vec<String>) -> Self {
        self.blocked_tools = Arc::new(blocked);
        self
    }

    /// GAP 11: flip global dry-run on/off. When on, every
    /// dispatch returns a preview instead of running the
    /// handler.
    pub fn with_global_dry_run(mut self, dry_run: bool) -> Self {
        self.global_dry_run = dry_run;
        self
    }

    /// GAP 12: install an evidence-capture sink. When `Some`,
    /// the dispatcher hands each completed action to the sink
    /// for state-before/state-after capture + diff computation.
    pub fn with_evidence_sink(mut self, sink: Arc<dyn EvidenceCaptureSink>) -> Self {
        self.evidence_sink = Some(sink);
        self
    }

    /// Borrow the transaction store handle (for tests).
    pub fn transaction_store(&self) -> Option<Arc<TransactionStore>> {
        self.transaction_store.clone()
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

    /// GAP 11 — rich dispatch that consults the three-tier
    /// classification, dedupes on idempotency keys, supports
    /// dry-run preview, and persists every call into the
    /// transaction store (when configured).
    ///
    /// Returns the handler's result string on success. On
    /// dry-run, the returned string is the JSON-encoded
    /// [`DryRunPreview`]. On idempotency hit, the cached
    /// result from the prior call is returned and the handler
    /// is NOT invoked.
    ///
    /// Existing call sites that use the bare [`Self::dispatch`]
    /// keep working unchanged.
    pub async fn dispatch_with_options<F, Fut>(
        &self,
        agent: &str,
        tool: &str,
        args: &str,
        mut options: GatewayDispatchOptions,
        handler: F,
    ) -> Result<String, DispatchError>
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = Result<String, String>>,
    {
        // 0a. Static Tier C list.
        if self.blocked_tools.iter().any(|t| t == tool) {
            return Err(DispatchError::AccessDenied(format!(
                "tool `{tool}` is on the gateway blocked list (Tier C)"
            )));
        }
        // 0b. Per-call Tier C.
        if let Some(GatewayTier::Blocked { reason }) = options.tier.as_ref() {
            return Err(DispatchError::AccessDenied(format!(
                "tool `{tool}` declared as Tier C: {reason}"
            )));
        }
        // 1. Access broker.
        match self.broker.check(agent, tool) {
            AccessDecision::Allow => {}
            AccessDecision::Deny { reason } => return Err(DispatchError::AccessDenied(reason)),
            AccessDecision::RateLimited { retry_after_secs } => {
                return Err(DispatchError::RateLimited { retry_after_secs });
            }
        }
        // 2. Idempotency dedup.
        if let Some(store) = self.transaction_store.as_ref()
            && let Some(key) = options.idempotency_key.as_deref()
            && let Ok(Some(prior)) = store.find_by_idempotency_key(tool, key)
        {
            // Replay the cached result. The handler is NOT
            // invoked.
            tracing::info!(
                tool,
                agent,
                idempotency_key = %key,
                action_id = %prior.action_id,
                "dispatch_with_options: idempotency hit; replaying prior result"
            );
            if let Some(out) = prior.result {
                return Ok(out);
            }
            if let Some(err) = prior.error {
                return Err(DispatchError::HandlerFailed(format!(
                    "idempotency replay: prior call failed: {err}"
                )));
            }
            // Fall through if the prior row has neither a
            // result nor an error — defensive.
        }
        // 3. Secret resolution.
        let resolved_args = match self.secret_store.resolve(args) {
            Ok(s) => s,
            Err(SecretError::Missing(name, _hint)) => {
                self.record_options_failure(
                    tool,
                    args,
                    &options,
                    format!("secret missing: {name}"),
                );
                return Err(DispatchError::SecretMissing(name));
            }
        };
        // 4. Dry-run preview path.
        let effective_dry_run = options.dry_run || self.global_dry_run;
        if effective_dry_run {
            options.dry_run = true;
            let tier = options
                .tier
                .clone()
                .unwrap_or(GatewayTier::HumanRollbackPlan {
                    rollback_plan: String::new(),
                });
            let preview = DryRunPreview::build(tool, &resolved_args, &tier);
            let payload = preview.to_json_string();
            self.record_options_success(
                tool,
                &resolved_args,
                Some(payload.clone()),
                &options,
                agent,
                None,
            );
            return Ok(payload);
        }
        // 5. Handler dispatch.
        let started = unix_millis();
        let result = handler(resolved_args.clone()).await;
        let completed = unix_millis();
        match result {
            Ok(output) => {
                let guard = ToolOutputGuard::inspect(&output);
                if guard.injection_detected {
                    let reason = guard
                        .reason
                        .clone()
                        .unwrap_or_else(|| "tool output flagged by guard".to_string());
                    self.record_options_failure(tool, &resolved_args, &options, reason.clone());
                    return Err(DispatchError::HandlerFailed(reason));
                }
                let safe_output = guard.output;
                self.record_options_success(
                    tool,
                    &resolved_args,
                    Some(safe_output.clone()),
                    &options,
                    agent,
                    Some((started, completed)),
                );
                self.broker.record_call(agent);
                Ok(safe_output)
            }
            Err(reason) => {
                self.record_options_failure(tool, &resolved_args, &options, reason.clone());
                Err(DispatchError::HandlerFailed(reason))
            }
        }
    }

    fn record_options_success(
        &self,
        tool: &str,
        args: &str,
        result: Option<String>,
        options: &GatewayDispatchOptions,
        agent: &str,
        timings: Option<(i64, i64)>,
    ) {
        let started_at_ms = timings.map(|(s, _)| s).unwrap_or_else(unix_millis);
        let completed_at_ms = timings.map(|(_, c)| c).unwrap_or_else(unix_millis);
        // Always update the legacy in-memory gateway so
        // existing callers reading via `gateway_snapshot()`
        // keep seeing the same shape.
        let mut action = GatewayAction::new(tool, args, legacy_reversible(&options.tier));
        if let Some(hint) = legacy_rollback_hint(&options.tier) {
            action = action.with_rollback_hint(hint);
        }
        if let Some(r) = result.clone() {
            action = action.with_result(r);
        }
        self.gateway.lock().unwrap().record_completed(action);
        // GAP 11 — persist if a store is wired.
        if let Some(store) = self.transaction_store.as_ref() {
            let row = build_success_row(
                tool,
                args,
                result.clone(),
                options,
                started_at_ms,
                completed_at_ms,
            );
            if let Err(e) = store.record(&row) {
                tracing::warn!(error = %e, tool, "dispatch_with_options: store record failed");
            }
            // GAP 12 — capture evidence.
            if let Some(sink) = self.evidence_sink.as_ref() {
                sink.capture(EvidenceCaptureCtx {
                    action: &row,
                    agent,
                    tool,
                    args,
                    result: result.as_deref(),
                    error: None,
                    started_at_ms,
                    completed_at_ms,
                });
            }
        }
    }

    fn record_options_failure(
        &self,
        tool: &str,
        args: &str,
        options: &GatewayDispatchOptions,
        error: String,
    ) {
        let completed_at_ms = unix_millis();
        let started_at_ms = completed_at_ms;
        let mut action = GatewayAction::new(tool, args, legacy_reversible(&options.tier));
        if let Some(hint) = legacy_rollback_hint(&options.tier) {
            action = action.with_rollback_hint(hint);
        }
        self.gateway.lock().unwrap().record_failed(action);
        if let Some(store) = self.transaction_store.as_ref() {
            let row = build_failure_row(
                tool,
                args,
                error.clone(),
                options,
                started_at_ms,
                completed_at_ms,
            );
            if let Err(e) = store.record(&row) {
                tracing::warn!(error = %e, tool, "dispatch_with_options: store record failed (failure path)");
            }
            if let Some(sink) = self.evidence_sink.as_ref() {
                sink.capture(EvidenceCaptureCtx {
                    action: &row,
                    agent: "",
                    tool,
                    args,
                    result: None,
                    error: Some(&error),
                    started_at_ms,
                    completed_at_ms,
                });
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

/// Map a [`GatewayTier`] back to the legacy `reversible: bool`
/// the in-memory `ActionGateway` uses for the dashboard.
fn legacy_reversible(tier: &Option<GatewayTier>) -> bool {
    match tier {
        Some(GatewayTier::AutoCompensated { .. }) => true,
        Some(GatewayTier::HumanRollbackPlan { rollback_plan }) => !rollback_plan.is_empty(),
        Some(GatewayTier::Blocked { .. }) => false,
        None => true,
    }
}

/// Pull the operator-readable rollback string out of a tier
/// so the legacy in-memory gateway keeps showing the hint.
fn legacy_rollback_hint(tier: &Option<GatewayTier>) -> Option<String> {
    match tier {
        Some(GatewayTier::AutoCompensated {
            compensating_tool, ..
        }) => Some(format!("auto-compensate via `{compensating_tool}`")),
        Some(GatewayTier::HumanRollbackPlan { rollback_plan }) if !rollback_plan.is_empty() => {
            Some(rollback_plan.clone())
        }
        _ => None,
    }
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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

    // ── GAP 11: dispatch_with_options ───────────────────────

    use super::super::super::execution::gateway_tier::GatewayDispatchOptions;
    use super::super::super::execution::transaction_store::TransactionStore;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn dispatcher_with_store() -> (ToolDispatcher, Arc<TransactionStore>) {
        let store = store_with(&[]);
        let broker = empty_broker();
        let tx_store = Arc::new(TransactionStore::open_in_memory().unwrap());
        let disp = ToolDispatcher::new(store, broker).with_transaction_store(tx_store.clone());
        (disp, tx_store)
    }

    #[tokio::test]
    async fn dispatch_with_options_persists_action_to_transaction_store() {
        let (disp, store) = dispatcher_with_store();
        let opts = GatewayDispatchOptions::default()
            .with_transaction_id("tx-test")
            .human_rollback_plan("rollback by hand");
        disp.dispatch_with_options("alice", "tool.x", r#"{"k":"v"}"#, opts, |args| async move {
            Ok(format!("handled-{args}"))
        })
        .await
        .unwrap();
        let rows = store.list_for_transaction("tx-test").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].success);
        assert!(rows[0].result.as_deref().unwrap().contains("handled-"));
        assert_eq!(rows[0].tier.tag(), "human_rollback");
    }

    #[tokio::test]
    async fn idempotency_key_skips_handler_on_retry() {
        let (disp, _store) = dispatcher_with_store();
        let calls = Arc::new(AtomicUsize::new(0));
        let opts = || {
            GatewayDispatchOptions::default()
                .with_transaction_id("tx-idem")
                .with_idempotency_key("k-1")
                .human_rollback_plan("hint")
        };
        // First call runs the handler.
        let c1 = calls.clone();
        let r1 = disp
            .dispatch_with_options("alice", "tool.x", "{}", opts(), |a| async move {
                c1.fetch_add(1, Ordering::SeqCst);
                Ok(format!("first:{a}"))
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(r1.contains("first:"));
        // Second call dedupes — handler is NOT invoked, the
        // cached result comes back instead.
        let c2 = calls.clone();
        let r2 = disp
            .dispatch_with_options("alice", "tool.x", "{}", opts(), |a| async move {
                c2.fetch_add(1, Ordering::SeqCst);
                Ok(format!("second:{a}"))
            })
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "handler invoked twice — idempotency failed"
        );
        assert_eq!(r1, r2);
    }

    #[tokio::test]
    async fn dry_run_returns_preview_without_invoking_handler() {
        let (disp, store) = dispatcher_with_store();
        let calls = Arc::new(AtomicUsize::new(0));
        let opts = GatewayDispatchOptions::default()
            .with_transaction_id("tx-dry")
            .auto_compensated("tool.x.undo", json!({"id": 1}))
            .dry_run();
        let c = calls.clone();
        let r = disp
            .dispatch_with_options("alice", "tool.x", r#"{"x":1}"#, opts, |_| async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok("real-call".into())
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["tier_tag"], "auto_compensated");
        assert_eq!(v["compensating_tool"], "tool.x.undo");
        // Row is persisted as dry_run.
        let rows = store.list_for_transaction("tx-dry").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].dry_run);
    }

    #[tokio::test]
    async fn statically_blocked_tool_is_refused_before_dispatch() {
        let store = store_with(&[]);
        let broker = empty_broker();
        let disp = ToolDispatcher::new(store, broker)
            .with_blocked_tools(vec!["tool.terminal.rm_rf".to_string()]);
        let opts = GatewayDispatchOptions::default();
        let err = disp
            .dispatch_with_options("alice", "tool.terminal.rm_rf", "{}", opts, |_| async {
                Ok("never".into())
            })
            .await
            .unwrap_err();
        match err {
            DispatchError::AccessDenied(r) => assert!(r.contains("blocked list")),
            other => panic!("expected AccessDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn per_call_blocked_tier_is_refused_before_dispatch() {
        let (disp, _store) = dispatcher_with_store();
        let opts = GatewayDispatchOptions::default().blocked("never permitted");
        let err = disp
            .dispatch_with_options("alice", "tool.x", "{}", opts, |_| async {
                Ok("nope".into())
            })
            .await
            .unwrap_err();
        match err {
            DispatchError::AccessDenied(r) => assert!(r.contains("never permitted")),
            other => panic!("expected AccessDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn legacy_dispatch_keeps_old_behaviour() {
        // Pre-GAP-11 callers using the bare `dispatch(...)` must
        // still get the existing in-memory ActionGateway shape.
        let store = store_with(&[]);
        let broker = empty_broker();
        let disp = ToolDispatcher::new(store, broker);
        disp.dispatch(
            "alice",
            "web.fetch",
            "{}",
            true,
            Some("re-fetch".into()),
            |_| async { Ok("body".into()) },
        )
        .await
        .unwrap();
        let snap = disp.gateway_snapshot();
        assert!(snap.contains("completed=1"));
        // No transaction store wired → in-memory only; no
        // transaction row appears anywhere.
        assert!(disp.transaction_store().is_none());
    }

    #[tokio::test]
    async fn global_dry_run_overrides_per_call_option() {
        let store = store_with(&[]);
        let broker = empty_broker();
        let tx_store = Arc::new(TransactionStore::open_in_memory().unwrap());
        let disp = ToolDispatcher::new(store, broker)
            .with_transaction_store(tx_store.clone())
            .with_global_dry_run(true);
        let opts = GatewayDispatchOptions::default()
            .with_transaction_id("tx-globaldry")
            .human_rollback_plan("plan");
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let r = disp
            .dispatch_with_options("alice", "tool.x", "{}", opts, |_| async move {
                c.fetch_add(1, Ordering::SeqCst);
                Ok("never".into())
            })
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let v: serde_json::Value = serde_json::from_str(&r).unwrap();
        assert_eq!(v["tier_tag"], "human_rollback");
    }
}
