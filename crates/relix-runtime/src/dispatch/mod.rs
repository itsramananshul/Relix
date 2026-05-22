//! Dispatch bridge — RELIX-1 §1.13 admission pipeline (alpha subset).
//!
//! The bridge owns:
//! - the capability registry (method → handler map),
//! - the policy engine,
//! - the trust root (org-root pubkey),
//! - the audit log.
//!
//! For every inbound `transport::rpc::Event::Request`, it runs the strict
//! admission pipeline (alpha steps from RELIX-1 §1.13: 1, 3, 5, 9, 10, 11)
//! and dispatches to the registered handler.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde_bytes::ByteBuf;

use relix_core::audit::{AuditDraft, AuditLog, AuditStatus};
use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_core::identity::{VerifiedIdentity, validate_identity_bundle};
use relix_core::policy::{Decision, PolicyEngine};
use relix_core::types::{ErrorEnvelope, NodeId, Timestamp, error_kinds};

use crate::transport::envelope::{RequestEnvelope, ResponseEnvelope, ResponseResult};

/// Context passed to a capability handler. Carries verified caller identity
/// and enough state for the handler to perform outbound calls and emit events.
pub struct InvocationCtx {
    /// Verified caller identity (post admission steps 5+9).
    pub caller: VerifiedIdentity,
    /// Trace context echoed in outbound calls.
    pub trace_id: relix_core::types::TraceId,
    /// The request id of the inbound call (echoed back).
    pub request_id: relix_core::types::RequestId,
    /// CBOR-encoded arguments.
    pub args: Vec<u8>,
}

/// Outcome a handler returns. Maps to `ResponseResult` on the wire.
pub enum HandlerOutcome {
    /// Encoded successful response body.
    Ok(Vec<u8>),
    /// Application-level error.
    Err(ErrorEnvelope),
}

/// PH-DISP1: internal outcome bucket for [`DispatchBridge::bump_stats`].
enum StatBucket {
    Ok,
    Err,
    Denied,
    Unknown,
}

/// W2-007d: one observation in the policy denial ring.
/// Cheap to clone — every field is owned + small.
#[derive(Debug, Clone)]
pub struct PolicyDenialEntry {
    /// Unix seconds when the denial was recorded.
    pub at: i64,
    /// Method the caller attempted.
    pub method: String,
    /// Caller's subject_id (hex). Same string the audit log
    /// records — operators can correlate.
    pub caller_subject_id: String,
    /// Caller's friendly name from their VerifiedIdentity.
    pub caller_name: String,
    /// Name of the policy rule that explicitly denied, or
    /// `"default_deny"` when nothing matched.
    pub rule: String,
    /// Operator-readable reason from the policy engine.
    pub reason: String,
}

/// W2-007d: bounded ring of recent [`PolicyDenialEntry`]s on
/// the local DispatchBridge. FIFO eviction; default capacity
/// 256. Resets on bridge restart.
#[derive(Debug)]
pub struct PolicyDenialRing {
    entries: std::sync::Mutex<std::collections::VecDeque<PolicyDenialEntry>>,
    capacity: usize,
}

/// W2-007d: default ring capacity. Same convention as the
/// other in-memory rings (fs / terminal / mcp audit).
pub const POLICY_DENIAL_RING_DEFAULT: usize = 256;

impl Default for PolicyDenialRing {
    fn default() -> Self {
        Self::new(POLICY_DENIAL_RING_DEFAULT)
    }
}

impl PolicyDenialRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: std::sync::Mutex::new(std::collections::VecDeque::with_capacity(capacity)),
            capacity: capacity.max(1),
        }
    }

    pub fn push(&self, e: PolicyDenialEntry) {
        let mut g = self.entries.lock().expect("policy denial ring poisoned");
        if g.len() == self.capacity {
            g.pop_front();
        }
        g.push_back(e);
    }

    /// Snapshot the most recent `max` entries, newest first.
    pub fn snapshot_newest_first(&self, max: usize) -> Vec<PolicyDenialEntry> {
        let g = self.entries.lock().expect("policy denial ring poisoned");
        g.iter().rev().take(max).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .expect("policy denial ring poisoned")
            .len()
    }

    #[allow(dead_code)] // pairs with len() per clippy len_zero
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A capability handler: native function invoked by the dispatch bridge.
#[async_trait]
pub trait Handler: Send + Sync {
    /// Invoke the handler. The dispatch bridge has already verified identity
    /// and policy; the handler need only execute the capability.
    async fn invoke(&self, ctx: InvocationCtx) -> HandlerOutcome;
}

/// Function-handler adapter. Lets a `Fn(InvocationCtx) -> Future<HandlerOutcome>`
/// be used without writing a struct impl every time.
pub struct FnHandler<F>(pub F);

#[async_trait]
impl<F, Fut> Handler for FnHandler<F>
where
    F: Fn(InvocationCtx) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = HandlerOutcome> + Send,
{
    async fn invoke(&self, ctx: InvocationCtx) -> HandlerOutcome {
        (self.0)(ctx).await
    }
}

/// The registry + admission pipeline. Constructed once at controller startup.
pub struct DispatchBridge {
    handlers: HashMap<String, Arc<dyn Handler>>,
    policy: PolicyEngine,
    trust_root: VerifyingKey,
    audit: tokio::sync::Mutex<AuditLog>,
    responder_node_id: NodeId,
    /// PH-DISP1: per-capability invocation counters. One row
    /// per method name the bridge has seen, populated as
    /// requests pass through the admission pipeline. Exposed
    /// via [`Self::capability_stats_snapshot`] for the bridge
    /// / dashboard to project. Pure observability; doesn't
    /// gate any decision.
    ///
    /// W2-006b: wrapped in `Arc` so handlers (e.g. the
    /// `node.dispatch.stats` capability the bridge exposes)
    /// can capture a cheap clone of the shared lock without
    /// needing access to the whole DispatchBridge.
    capability_stats: Arc<std::sync::RwLock<HashMap<String, CapStats>>>,
    /// W2-007d: bounded ring of recent policy denials. The
    /// admission step pushes one entry on every Deny outcome
    /// before the audit log is written. Surfaced via the
    /// built-in `node.policy.recent_denials` capability.
    policy_denials: Arc<PolicyDenialRing>,
    /// Optional agent-employee gate plumbing. Wired by the
    /// coordinator binary at startup. `None` on every other
    /// node — those nodes skip the gate step entirely and
    /// preserve today's behavior.
    agent_gate: Option<AgentGateBindings>,
}

/// Describe a capability by method name. The gate uses this
/// for the risk-ceiling + categories check. Returns `None`
/// when the bridge has no metadata for the method (gate
/// falls back to a category-free, risk-free admit).
pub type CapabilityDescribeFn =
    Arc<dyn Fn(&str) -> Option<relix_core::capability::CapabilityDescriptor> + Send + Sync>;

/// Coordinator-side closure that records an approval request
/// when the gate returns `RequireApproval`. Implementation
/// mints the approval row + chronicle event + telegram
/// notification. Returns the new approval_id.
pub type OnRequireApprovalFn = Arc<
    dyn Fn(&crate::admission::agent_gate::GateApprovalRequest, &str) -> Result<String, String>
        + Send
        + Sync,
>;

/// What the bridge needs to run the agent gate.
#[derive(Clone)]
pub struct AgentGateBindings {
    /// Read-only store handle for the categorical lookups.
    pub store: crate::admission::agent_gate::AgentStoreHandle,
    /// Closure the gate uses to look up a descriptor for the
    /// method being called.
    pub describe: CapabilityDescribeFn,
    /// Closure that records an approval row + chronicle
    /// event + telegram fire when the gate returns
    /// `RequireApproval`. Returns the new approval_id.
    pub on_require_approval: OnRequireApprovalFn,
}

/// PH-DISP1: per-capability counters. Counts are lifetime —
/// reset on bridge restart. The dashboard renders these via a
/// future projection; today they're queryable in-process.
///
/// W2-006a extends the counters with latency fields:
/// `last_elapsed_ms`, `total_elapsed_ms`, `max_elapsed_ms`, and
/// `latency_samples`. Mean latency is `total_elapsed_ms /
/// latency_samples` (callers compute it; this struct stays a
/// dumb counter bag). Latency captures only successful or
/// handler-errored invocations — policy-denied / unknown-method
/// attempts don't reach the handler so they have no elapsed
/// time to record.
#[derive(Debug, Default, Clone)]
pub struct CapStats {
    /// Total successful invocations (handler returned Ok).
    pub invocations: u64,
    /// Total handler-level errors (handler returned Err).
    pub errors: u64,
    /// Total policy-denied attempts. Never reaches the handler.
    pub denied: u64,
    /// Total unknown-method attempts. These don't have a
    /// registered handler so the counter lives under the
    /// caller-supplied method name; useful for spotting
    /// mistyped capability names.
    pub unknown_method: u64,
    /// Wall-clock unix seconds of the most recent invocation
    /// outcome (Ok or Err — set to `now` on every dispatch).
    pub last_invoked_at: i64,
    /// Wall-clock unix seconds of the most recent error
    /// (handler Err OR policy-denied OR unknown_method).
    pub last_error_at: Option<i64>,
    /// W2-006a: elapsed_ms of the most recent dispatched
    /// invocation (Ok or Err). 0 when no invocation has
    /// completed yet — distinguishable from a real 0ms call by
    /// `latency_samples == 0`.
    pub last_elapsed_ms: u64,
    /// W2-006a: rolling max of the per-call elapsed_ms across
    /// every Ok or Err invocation. Useful for "is anything
    /// hanging?" at-a-glance.
    pub max_elapsed_ms: u64,
    /// W2-006a: sum of elapsed_ms across every Ok or Err
    /// invocation. Divide by `latency_samples` for mean.
    /// Saturates on overflow (u64 is more than enough for
    /// realistic operator workloads, but the saturating
    /// arithmetic stays defensive).
    pub total_elapsed_ms: u64,
    /// W2-006a: number of Ok+Err invocations recorded — the
    /// denominator for mean latency. Distinct from
    /// `invocations + errors` only because policy-denied /
    /// unknown_method don't contribute (no handler call → no
    /// elapsed time).
    pub latency_samples: u64,
    /// W2-006d: bounded ring of the most-recent per-call
    /// elapsed_ms values (newest at the back). Capacity
    /// [`RECENT_LATENCIES_CAP`]; FIFO eviction. Powers the
    /// dashboard's inline sparkline so operators see latency
    /// shape (steady? spiky? climbing?) without staring at
    /// just last/mean/max numbers.
    pub recent_latencies: std::collections::VecDeque<u32>,
}

/// W2-006d: how many recent per-call latency samples to keep
/// per capability. 32 is enough to draw a meaningful
/// sparkline at the dashboard's natural column width without
/// bloating the per-row footprint.
pub const RECENT_LATENCIES_CAP: usize = 32;

impl DispatchBridge {
    /// Construct.
    pub fn new(
        policy: PolicyEngine,
        trust_root: VerifyingKey,
        audit_path: &std::path::Path,
        responder_signer: SigningKey,
    ) -> Result<Self, DispatchError> {
        let responder_node_id = NodeId::from_pubkey(&responder_signer.verifying_key().to_bytes());
        let audit = AuditLog::open(audit_path, responder_signer)
            .map_err(|e| DispatchError::AuditOpen(e.to_string()))?;
        Ok(Self {
            handlers: HashMap::new(),
            policy,
            trust_root,
            audit: tokio::sync::Mutex::new(audit),
            responder_node_id,
            capability_stats: Arc::new(std::sync::RwLock::new(HashMap::new())),
            policy_denials: Arc::new(PolicyDenialRing::default()),
            agent_gate: None,
        })
    }

    /// Wire the agent-employee gate. Called by the coordinator
    /// binary after the [`crate::nodes::coordinator::agent::AgentStore`]
    /// is open. No-op on nodes that don't host an agent store.
    pub fn set_agent_gate(&mut self, bindings: AgentGateBindings) {
        self.agent_gate = Some(bindings);
    }

    /// W2-007d: cheap-clone accessor for the policy denial
    /// ring. Used by the built-in `node.policy.recent_denials`
    /// capability + future bridge proxy.
    pub fn policy_denials_handle(&self) -> Arc<PolicyDenialRing> {
        self.policy_denials.clone()
    }

    /// W2-006b: return a cheap clone of the capability-stats
    /// RwLock handle. Handlers registered against this bridge
    /// (e.g. the built-in `node.dispatch.stats`) capture this
    /// to read the snapshot without owning the bridge.
    pub fn capability_stats_handle(&self) -> Arc<std::sync::RwLock<HashMap<String, CapStats>>> {
        self.capability_stats.clone()
    }

    /// W2-007a: return a clone of the PolicyEngine. Used by the
    /// `node.policy.simulate` built-in capability — handlers
    /// can answer "what would the policy say?" questions
    /// without owning the bridge.
    pub fn policy_handle(&self) -> PolicyEngine {
        self.policy.clone()
    }

    /// PH-DISP1: snapshot of every capability's counters.
    /// Order is stable (by method name) so dashboards diff
    /// cleanly across calls. Returns an empty vec when no
    /// requests have been dispatched yet.
    pub fn capability_stats_snapshot(&self) -> Vec<(String, CapStats)> {
        let g = self
            .capability_stats
            .read()
            .expect("capability_stats read lock");
        let mut out: Vec<(String, CapStats)> =
            g.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// PH-DISP1: internal helper. Bumps the counter row for
    /// `method` according to the outcome bucket.
    fn bump_stats(&self, method: &str, bucket: StatBucket, now: i64) {
        self.bump_stats_with_latency(method, bucket, now, None);
    }

    /// W2-006a: variant that also records per-call elapsed_ms
    /// for Ok / Err invocations. Denied / Unknown buckets don't
    /// have a handler call so the elapsed argument is ignored
    /// (callers may still pass `Some` for ergonomics; we skip
    /// the update).
    fn bump_stats_with_latency(
        &self,
        method: &str,
        bucket: StatBucket,
        now: i64,
        elapsed_ms: Option<u64>,
    ) {
        let mut g = self
            .capability_stats
            .write()
            .expect("capability_stats write lock");
        let row = g.entry(method.to_string()).or_default();
        row.last_invoked_at = now;
        match bucket {
            StatBucket::Ok => {
                row.invocations = row.invocations.saturating_add(1);
            }
            StatBucket::Err => {
                row.errors = row.errors.saturating_add(1);
                row.last_error_at = Some(now);
            }
            StatBucket::Denied => {
                row.denied = row.denied.saturating_add(1);
                row.last_error_at = Some(now);
            }
            StatBucket::Unknown => {
                row.unknown_method = row.unknown_method.saturating_add(1);
                row.last_error_at = Some(now);
            }
        }
        // Latency only meaningful for Ok / Err (handler ran).
        if matches!(bucket, StatBucket::Ok | StatBucket::Err)
            && let Some(ms) = elapsed_ms
        {
            row.last_elapsed_ms = ms;
            row.max_elapsed_ms = row.max_elapsed_ms.max(ms);
            row.total_elapsed_ms = row.total_elapsed_ms.saturating_add(ms);
            row.latency_samples = row.latency_samples.saturating_add(1);
            // W2-006d: push into the bounded ring (clamp to
            // u32 to keep the wire payload compact — anyone
            // with a single-call latency > 49 days has bigger
            // problems than a saturating cast).
            let ms_u32 = u32::try_from(ms).unwrap_or(u32::MAX);
            if row.recent_latencies.len() == RECENT_LATENCIES_CAP {
                row.recent_latencies.pop_front();
            }
            row.recent_latencies.push_back(ms_u32);
        }
    }

    /// Register a capability handler.
    pub fn register(&mut self, method: impl Into<String>, handler: Arc<dyn Handler>) {
        self.handlers.insert(method.into(), handler);
    }

    /// Run the admission pipeline on an inbound encoded envelope and dispatch.
    /// Returns the encoded response envelope to send back on the wire.
    pub async fn handle_inbound(&self, encoded_envelope: Vec<u8>) -> Vec<u8> {
        let started_at = Instant::now();

        // === Admission step 1: decode envelope ===
        let req: RequestEnvelope = match codec::decode(&encoded_envelope) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "admission step 1 decode failed");
                return encode_error_response_no_audit(
                    relix_core::types::RequestId([0u8; 16]),
                    self.responder_node_id,
                    error_kinds::INVALID_ARGS,
                    "envelope decode failed",
                );
            }
        };

        // === Admission step 3: deadline ===
        let now = unix_now();
        if now > req.deadline.0 + 30 {
            return self
                .audit_and_err(
                    req,
                    started_at,
                    "admission:deadline_exceeded",
                    error_kinds::TIMEOUT,
                )
                .await;
        }

        // === Admission step 5: verify identity bundle ===
        let verified = match validate_identity_bundle(&req.identity_bundle, &self.trust_root, now) {
            Ok(v) => v,
            Err(e) => {
                return self
                    .audit_and_err_unverified(
                        &req,
                        started_at,
                        format!("admission:identity_invalid:{e}"),
                        error_kinds::IDENTITY_INVALID,
                    )
                    .await;
            }
        };

        // === Admission step 7: capability lookup ===
        let Some(handler) = self.handlers.get(&req.method).cloned() else {
            // PH-DISP1: count even unknown-method attempts so
            // operators can spot mistyped capability names in
            // the dashboard (e.g. "task.todo_set" vs the typo
            // "task.todo_create").
            self.bump_stats(&req.method, StatBucket::Unknown, now);
            return self
                .audit_and_err_with_id(
                    &req,
                    &verified,
                    started_at,
                    "admission:unknown_method".into(),
                    error_kinds::UNKNOWN_METHOD,
                    AuditStatus::Error,
                )
                .await;
        };

        // === Admission step 8: agent-employee gate (categorical / surface / risk / approval) ===
        if let Some(bindings) = self.agent_gate.as_ref() {
            let descriptor = (bindings.describe)(&req.method);
            let gate_decision = crate::admission::agent_gate::evaluate(
                Some(&bindings.store),
                crate::admission::agent_gate::GateInputs {
                    identity: &verified,
                    envelope: &req,
                    capability: descriptor.as_ref(),
                    now,
                },
            );
            match gate_decision {
                crate::admission::agent_gate::GateDecision::Allow(a) => {
                    if let Some(approval_id) = a.consumed_approval_id.as_deref() {
                        // Token admit → consume the one-shot.
                        if let Some(token) = req.approval_token.as_deref()
                            && let Err(e) = bindings.store.consume_approval_token(token)
                        {
                            tracing::warn!(
                                approval_id = %approval_id,
                                error = %e,
                                "agent_gate: approval token consume failed"
                            );
                        }
                    }
                }
                crate::admission::agent_gate::GateDecision::Deny(deny) => {
                    self.bump_stats(&req.method, StatBucket::Denied, now);
                    self.policy_denials.push(PolicyDenialEntry {
                        at: now,
                        method: req.method.clone(),
                        caller_subject_id: verified.subject_id.to_string(),
                        caller_name: verified.name.clone(),
                        rule: deny.matched_rule.clone(),
                        reason: deny.reason.clone(),
                    });
                    return self
                        .audit_and_err_with_id(
                            &req,
                            &verified,
                            started_at,
                            format!("agent_gate:deny:{}:{}", deny.matched_rule, deny.reason),
                            relix_core::types::error_kinds::POLICY_DENIED,
                            AuditStatus::Denied,
                        )
                        .await;
                }
                crate::admission::agent_gate::GateDecision::RequireApproval(req_appr) => {
                    self.bump_stats(&req.method, StatBucket::Denied, now);
                    // The GateApprovalRequest already carries the
                    // task_id from the envelope (or None when the
                    // caller didn't supply one). Pass it through
                    // for symmetry with the closure signature; the
                    // closure prefers `req_appr.task_id`.
                    let task_id_hint = req_appr.task_id.as_deref().unwrap_or("");
                    let cause = match (bindings.on_require_approval)(&req_appr, task_id_hint) {
                        Ok(approval_id) => format!("approval_required:{approval_id}"),
                        Err(e) => format!("approval_required (create failed: {e})"),
                    };
                    return self
                        .audit_and_err_with_id(
                            &req,
                            &verified,
                            started_at,
                            cause,
                            relix_core::types::error_kinds::APPROVAL_REQUIRED,
                            AuditStatus::Denied,
                        )
                        .await;
                }
            }
        }

        // === Admission step 9: policy ===
        let decision = self.policy.evaluate(&verified, &req.method);
        let (policy_decision_str, denied) = match &decision {
            Decision::Allow { matched_rule } => (format!("allow:{matched_rule}"), false),
            Decision::Deny {
                reason,
                matched_rule,
            } => (
                format!(
                    "deny:{}:{}",
                    matched_rule.as_deref().unwrap_or("default_deny"),
                    reason
                ),
                true,
            ),
        };
        if denied {
            self.bump_stats(&req.method, StatBucket::Denied, now);
            // W2-007d: capture the structured denial for the
            // operator-facing ring. Pulls the rule / reason
            // out of the `decision` match arm rather than
            // re-parsing the joined string. The audit log
            // still records the canonical line; this ring is
            // a fast read surface.
            if let Decision::Deny {
                reason,
                matched_rule,
            } = &decision
            {
                self.policy_denials.push(PolicyDenialEntry {
                    at: now,
                    method: req.method.clone(),
                    caller_subject_id: verified.subject_id.to_string(),
                    caller_name: verified.name.clone(),
                    rule: matched_rule
                        .clone()
                        .unwrap_or_else(|| "default_deny".to_string()),
                    reason: reason.clone(),
                });
            }
            return self
                .audit_and_err_with_id(
                    &req,
                    &verified,
                    started_at,
                    policy_decision_str,
                    error_kinds::POLICY_DENIED,
                    AuditStatus::Denied,
                )
                .await;
        }

        // === Admission step 10: dispatch ===
        let ctx = InvocationCtx {
            caller: verified.clone(),
            trace_id: req.tid,
            request_id: req.rid,
            args: req.args.to_vec(),
        };
        // W2-006a: capture per-call elapsed_ms. Instant::now
        // straddles only the handler invocation — admission /
        // policy / audit are explicitly NOT included so the
        // operator-visible latency reflects user code, not the
        // bridge's overhead.
        let dispatch_started = std::time::Instant::now();
        let outcome = handler.invoke(ctx).await;
        let elapsed_ms = dispatch_started.elapsed().as_millis().min(u64::MAX as u128) as u64;

        // === Admission step 11: audit ===
        let (result, status, error_kind) = match outcome {
            HandlerOutcome::Ok(body) => (
                ResponseResult::Ok(ByteBuf::from(body)),
                AuditStatus::Ok,
                None,
            ),
            HandlerOutcome::Err(e) => (
                ResponseResult::Err(e.clone()),
                AuditStatus::Error,
                Some(e.kind),
            ),
        };
        // PH-DISP1: count the dispatched outcome.
        // W2-006a: also record latency for Ok / Err.
        let bucket = if matches!(status, AuditStatus::Ok) {
            StatBucket::Ok
        } else {
            StatBucket::Err
        };
        self.bump_stats_with_latency(&req.method, bucket, now, Some(elapsed_ms));
        let aid = self
            .write_audit(
                &req,
                &verified,
                started_at,
                policy_decision_str,
                status,
                error_kind,
            )
            .await;
        let resp = ResponseEnvelope {
            pv: 1,
            rid: req.rid,
            responder: self.responder_node_id,
            res: result,
            aid: ByteBuf::from(aid),
            processed_at: Timestamp::now(),
        };
        codec::encode(&resp).unwrap_or_default()
    }

    async fn audit_and_err(
        &self,
        req: RequestEnvelope,
        started: Instant,
        decision: &str,
        error_kind: u32,
    ) -> Vec<u8> {
        // Caller is "unknown" — best effort: zero claims.
        let unknown = unknown_identity();
        let aid = self
            .write_audit(
                &req,
                &unknown,
                started,
                decision.to_string(),
                AuditStatus::Error,
                Some(error_kind),
            )
            .await;
        encode_error_response(req.rid, self.responder_node_id, aid, error_kind, decision)
    }

    async fn audit_and_err_unverified(
        &self,
        req: &RequestEnvelope,
        started: Instant,
        decision: String,
        error_kind: u32,
    ) -> Vec<u8> {
        let unknown = unknown_identity();
        let aid = self
            .write_audit(
                req,
                &unknown,
                started,
                decision.clone(),
                AuditStatus::Error,
                Some(error_kind),
            )
            .await;
        encode_error_response(req.rid, self.responder_node_id, aid, error_kind, &decision)
    }

    async fn audit_and_err_with_id(
        &self,
        req: &RequestEnvelope,
        caller: &VerifiedIdentity,
        started: Instant,
        decision: String,
        error_kind: u32,
        status: AuditStatus,
    ) -> Vec<u8> {
        let aid = self
            .write_audit(
                req,
                caller,
                started,
                decision.clone(),
                status,
                Some(error_kind),
            )
            .await;
        encode_error_response(req.rid, self.responder_node_id, aid, error_kind, &decision)
    }

    async fn write_audit(
        &self,
        req: &RequestEnvelope,
        caller: &VerifiedIdentity,
        started: Instant,
        decision: String,
        status: AuditStatus,
        error_kind: Option<u32>,
    ) -> Vec<u8> {
        let draft = AuditDraft {
            request_id: req.rid,
            trace_id: req.tid,
            caller_node_id: caller.subject_id,
            caller_name: caller.name.clone(),
            caller_groups: caller.groups.clone(),
            method: req.method.clone(),
            flow_id: None,
            started_at: started,
        };
        let aid = req.rid.0.to_vec(); // alpha: use rid as audit id (cross-correlation key)
        let mut audit = self.audit.lock().await;
        if let Err(e) = audit.finalize(draft, decision, status, error_kind) {
            tracing::error!(error = %e, "audit write failed");
        }
        aid
    }
}

fn unknown_identity() -> VerifiedIdentity {
    VerifiedIdentity {
        subject_id: NodeId([0u8; 32]),
        name: "<unverified>".into(),
        org_id: NodeId([0u8; 32]),
        groups: vec![],
        role: "<unverified>".into(),
        clearance: "<unverified>".into(),
        bundle_id: [0u8; 32],
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn encode_error_response(
    rid: relix_core::types::RequestId,
    responder: NodeId,
    aid: Vec<u8>,
    kind: u32,
    cause: &str,
) -> Vec<u8> {
    let resp = ResponseEnvelope {
        pv: 1,
        rid,
        responder,
        res: ResponseResult::Err(ErrorEnvelope {
            kind,
            cause: cause.to_string(),
            retry_hint: 2,
            retry_after: None,
        }),
        aid: ByteBuf::from(aid),
        processed_at: Timestamp::now(),
    };
    codec::encode(&resp).unwrap_or_default()
}

fn encode_error_response_no_audit(
    rid: relix_core::types::RequestId,
    responder: NodeId,
    kind: u32,
    cause: &str,
) -> Vec<u8> {
    encode_error_response(rid, responder, vec![], kind, cause)
}

/// Build an outbound request envelope ready to send via `transport::rpc::Client::call`.
pub fn build_request(
    method: impl Into<String>,
    args: Vec<u8>,
    identity: Bundle,
    deadline_secs_from_now: i64,
) -> Vec<u8> {
    build_request_with_surface(
        method,
        args,
        identity,
        deadline_secs_from_now,
        None,
        None,
        None,
    )
}

/// Same as [`build_request`] but stamps the optional
/// `surface` + `approval_token` + `task_id` fields on the
/// envelope. Used by the bridge to mark which inbound HTTP
/// surface drove the call, by retried callers replaying an
/// approved approval token, and by callers acting on behalf
/// of a specific coordinator task (so the agent gate's
/// `RequireApproval` path can pause + resume the right
/// task).
#[allow(clippy::too_many_arguments)]
pub fn build_request_with_surface(
    method: impl Into<String>,
    args: Vec<u8>,
    identity: Bundle,
    deadline_secs_from_now: i64,
    surface: Option<String>,
    approval_token: Option<String>,
    task_id: Option<String>,
) -> Vec<u8> {
    let req = RequestEnvelope {
        pv: 1,
        rid: relix_core::types::RequestId::new(),
        tid: relix_core::types::TraceId::new(),
        method: method.into(),
        mv: 1,
        args: ByteBuf::from(args),
        identity_bundle: identity,
        deadline: Timestamp::now().add_secs(deadline_secs_from_now),
        surface,
        approval_token,
        task_id,
    };
    codec::encode(&req).unwrap_or_default()
}

/// Decode a response envelope returned by `Client::call`.
pub fn decode_response(bytes: &[u8]) -> Result<ResponseEnvelope, codec::CodecError> {
    codec::decode(bytes)
}

/// Dispatch-layer errors.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// Audit log could not be opened.
    #[error("audit open: {0}")]
    AuditOpen(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use relix_core::identity::{IdentityBundle, issue_identity};
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn echo_handler(ctx: InvocationCtx) -> HandlerOutcome {
        HandlerOutcome::Ok(ctx.args)
    }

    #[tokio::test]
    async fn admission_allow_path() {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "any_caller_echo"
            method = "node.health"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let mut bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder.clone(),
        )
        .unwrap();
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &org_root, 3600).unwrap();
        let envelope = build_request("node.health", b"hi".to_vec(), bundle, 30);

        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Ok(b) => assert_eq!(b.as_ref(), b"hi"),
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn admission_policy_denied() {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "chat_users_only"
            method = "node.health"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let mut bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "bob".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["guest".into()],
            role: "agent".into(),
            clearance: "public".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &org_root, 3600).unwrap();
        let envelope = build_request("node.health", b"hi".to_vec(), bundle, 30);

        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(e) => {
                assert_eq!(e.kind, error_kinds::POLICY_DENIED);
            }
            other => panic!("expected Err(policy_denied), got {:?}", other),
        }
    }

    /// Build a (caller-key, signed-bundle) pair for tests.
    fn mk_identity(org_root: &SigningKey, name: &str, groups: &[&str]) -> Bundle {
        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: name.into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: groups.iter().map(|s| s.to_string()).collect(),
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        issue_identity(id, org_root, 3600).unwrap()
    }

    /// Build a permissive bridge that always allows `node.health`.
    fn fresh_bridge(audit_dir: &TempDir) -> (DispatchBridge, SigningKey) {
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "anyone_health"
            method = "node.health"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &audit_dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        (bridge, org_root)
    }

    #[tokio::test]
    async fn response_rid_echoes_request_rid() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);
        // Pluck rid out of the envelope we just built.
        let parsed: RequestEnvelope = codec::decode(&envelope).unwrap();
        let sent_rid = parsed.rid;

        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        assert_eq!(sent_rid, resp.rid, "response rid must echo request rid");
    }

    #[tokio::test]
    async fn audit_record_written_on_success() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);

        let _ = bridge.handle_inbound(envelope).await;
        let recs = relix_core::audit::read_audit_records(dir.path().join("audit.log")).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].status, "ok");
        assert_eq!(recs[0].method, "node.health");
        assert!(recs[0].policy_decision.starts_with("allow:"));
    }

    #[tokio::test]
    async fn audit_record_written_on_denial() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let bundle = mk_identity(&org_root, "bob", &["guest"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);

        let _ = bridge.handle_inbound(envelope).await;
        let recs = relix_core::audit::read_audit_records(dir.path().join("audit.log")).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].status, "denied");
        assert_eq!(recs[0].method, "node.health");
        assert!(recs[0].policy_decision.starts_with("deny:"));
        assert_eq!(recs[0].error_kind, Some(error_kinds::POLICY_DENIED));
    }

    #[tokio::test]
    async fn handler_not_called_when_policy_denies() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);

        // Handler increments a counter every time it's invoked. If admission
        // is correct, the counter MUST stay at zero for a denied identity.
        let counter = Arc::new(AtomicU32::new(0));
        let counter_for_handler = counter.clone();
        bridge.register(
            "node.health",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let c = counter_for_handler.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    HandlerOutcome::Ok(b"ran".to_vec())
                }
            })),
        );

        let bundle = mk_identity(&org_root, "bob", &["guest"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);

        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(e) => assert_eq!(e.kind, error_kinds::POLICY_DENIED),
            other => panic!("expected denial, got {:?}", other),
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "handler must not have been called when policy denied"
        );
    }

    #[tokio::test]
    async fn handler_not_called_when_identity_invalid() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let dir = TempDir::new().unwrap();
        let real_root = SigningKey::generate(&mut OsRng);
        let attacker_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let mut bridge = DispatchBridge::new(
            PolicyEngine::permissive(),
            real_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();

        let counter = Arc::new(AtomicU32::new(0));
        let counter_for_handler = counter.clone();
        bridge.register(
            "node.health",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let c = counter_for_handler.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    HandlerOutcome::Ok(b"ran".to_vec())
                }
            })),
        );

        // Identity bundle signed by attacker_root — bridge trusts real_root.
        let bundle = mk_identity(&attacker_root, "intruder", &["chat-users"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);

        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(e) => assert_eq!(e.kind, error_kinds::IDENTITY_INVALID),
            other => panic!("expected identity_invalid, got {:?}", other),
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "handler must not have been called when identity invalid"
        );
    }

    #[tokio::test]
    async fn tampered_identity_bundle_rejected() {
        use serde_bytes::ByteBuf;
        let dir = TempDir::new().unwrap();
        let (bridge, org_root) = fresh_bridge(&dir);

        // Issue a valid bundle, then flip a payload byte.
        let mut bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let mut payload = bundle.payload.to_vec();
        let mid = payload.len() / 2;
        payload[mid] ^= 0xFF;
        bundle.payload = ByteBuf::from(payload);

        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(e) => assert_eq!(e.kind, error_kinds::IDENTITY_INVALID),
            other => panic!("expected identity_invalid, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn admission_wrong_trust_root() {
        let dir = TempDir::new().unwrap();
        let real_root = SigningKey::generate(&mut OsRng);
        let attacker_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let bridge = DispatchBridge::new(
            PolicyEngine::permissive(),
            real_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();

        // Bundle signed by attacker_root, but bridge trusts real_root.
        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&attacker_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &attacker_root, 3600).unwrap();
        let envelope = build_request("node.health", b"hi".to_vec(), bundle, 30);

        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(e) => assert_eq!(e.kind, error_kinds::IDENTITY_INVALID),
            other => panic!("expected Err(identity_invalid), got {:?}", other),
        }
    }

    // ── W2-007d: policy denial ring ─────────────────────────────────

    #[test]
    fn policy_denial_ring_default_capacity_matches_const() {
        let r = PolicyDenialRing::default();
        assert!(r.is_empty());
        // Push capacity + 50 entries; ring should saturate at default.
        for i in 0..(POLICY_DENIAL_RING_DEFAULT + 50) {
            r.push(PolicyDenialEntry {
                at: i as i64,
                method: "m".into(),
                caller_subject_id: "x".into(),
                caller_name: "x".into(),
                rule: "default_deny".into(),
                reason: "no rule".into(),
            });
        }
        assert_eq!(r.len(), POLICY_DENIAL_RING_DEFAULT);
    }

    #[test]
    fn policy_denial_ring_snapshot_returns_newest_first() {
        let r = PolicyDenialRing::default();
        for i in 0..3 {
            r.push(PolicyDenialEntry {
                at: 100 + i as i64,
                method: format!("m{i}"),
                caller_subject_id: "x".into(),
                caller_name: "x".into(),
                rule: "default_deny".into(),
                reason: "no rule".into(),
            });
        }
        let snap = r.snapshot_newest_first(10);
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].method, "m2");
        assert_eq!(snap[1].method, "m1");
        assert_eq!(snap[2].method, "m0");
    }

    #[test]
    fn policy_denial_ring_zero_capacity_clamps_to_one() {
        let r = PolicyDenialRing::new(0);
        r.push(PolicyDenialEntry {
            at: 1,
            method: "a".into(),
            caller_subject_id: "x".into(),
            caller_name: "x".into(),
            rule: "default_deny".into(),
            reason: "no rule".into(),
        });
        r.push(PolicyDenialEntry {
            at: 2,
            method: "b".into(),
            caller_subject_id: "x".into(),
            caller_name: "x".into(),
            rule: "default_deny".into(),
            reason: "no rule".into(),
        });
        // capacity clamped to 1 → only newest survives.
        assert_eq!(r.len(), 1);
        let snap = r.snapshot_newest_first(10);
        assert_eq!(snap[0].method, "b");
    }

    #[tokio::test]
    async fn policy_denial_pushes_to_ring_on_deny() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));
        // Caller in `guest` group; policy requires `chat-users`.
        let bundle = mk_identity(&org_root, "bob", &["guest"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);
        let _ = bridge.handle_inbound(envelope).await;
        // Ring must now have one entry.
        let snap = bridge.policy_denials_handle().snapshot_newest_first(10);
        assert_eq!(snap.len(), 1);
        let entry = &snap[0];
        assert_eq!(entry.method, "node.health");
        assert_eq!(entry.caller_name, "bob");
        // Either a named rule denied OR default_deny when no
        // rule matched. The test policy has a single rule
        // requiring `chat-users`, so default_deny is the
        // expected reason.
        assert!(entry.rule == "default_deny" || !entry.rule.is_empty());
        assert!(!entry.reason.is_empty());
    }

    #[tokio::test]
    async fn policy_denial_ring_empty_when_no_denial() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));
        // Caller in the allowed group — admission succeeds.
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);
        let resp = bridge.handle_inbound(envelope).await;
        let decoded = decode_response(&resp).unwrap();
        assert!(matches!(decoded.res, ResponseResult::Ok(_)));
        // Ring must still be empty.
        assert!(bridge.policy_denials_handle().is_empty());
    }

    // ── PH-DISP1: capability invocation counters ────────────────────────

    #[tokio::test]
    async fn capability_stats_counts_ok_invocations() {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "any"
            method = "node.health"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let mut bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &org_root, 3600).unwrap();
        for _ in 0..3 {
            let envelope = build_request("node.health", b"x".to_vec(), bundle.clone(), 30);
            let _ = bridge.handle_inbound(envelope).await;
        }
        let snap = bridge.capability_stats_snapshot();
        let (name, stats) = snap
            .iter()
            .find(|(n, _)| n == "node.health")
            .expect("node.health counter must exist");
        assert_eq!(name, "node.health");
        assert_eq!(stats.invocations, 3);
        assert_eq!(stats.errors, 0);
        assert_eq!(stats.denied, 0);
        assert_eq!(stats.unknown_method, 0);
        assert!(stats.last_invoked_at > 0);
        assert!(stats.last_error_at.is_none());
        // W2-006a: latency fields populated by every Ok dispatch.
        assert_eq!(stats.latency_samples, 3);
        assert!(stats.total_elapsed_ms >= stats.last_elapsed_ms);
        assert!(stats.max_elapsed_ms >= stats.last_elapsed_ms);
        // W2-006d: recent latency ring tracks the same 3
        // Ok dispatches.
        assert_eq!(stats.recent_latencies.len(), 3);
    }

    /// W2-006d: the recent-latencies ring must cap at
    /// RECENT_LATENCIES_CAP regardless of how many Ok / Err
    /// dispatches land. FIFO eviction means the *newest*
    /// sample wins over the *oldest*, not the other way
    /// around.
    #[tokio::test]
    async fn capability_stats_caps_recent_latencies_ring() {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "any"
            method = "node.health"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let mut bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &org_root, 3600).unwrap();
        // Dispatch CAP + 5 invocations so the ring has to
        // evict the first 5.
        let total = RECENT_LATENCIES_CAP + 5;
        for _ in 0..total {
            let envelope = build_request("node.health", b"x".to_vec(), bundle.clone(), 30);
            let _ = bridge.handle_inbound(envelope).await;
        }
        let snap = bridge.capability_stats_snapshot();
        let (_, stats) = snap
            .iter()
            .find(|(n, _)| n == "node.health")
            .expect("node.health counter must exist");
        assert_eq!(stats.recent_latencies.len(), RECENT_LATENCIES_CAP);
        // Total samples counter is uncapped — it still
        // reflects every Ok dispatch.
        assert_eq!(stats.latency_samples as usize, total);
    }

    #[tokio::test]
    async fn capability_stats_counts_unknown_method_attempts() {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "any"
            method = "anything.at.all"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        // NO handlers registered. Every call should bump
        // unknown_method.
        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &org_root, 3600).unwrap();
        let envelope = build_request("task.todo_typooo", b"".to_vec(), bundle, 30);
        let _ = bridge.handle_inbound(envelope).await;

        let snap = bridge.capability_stats_snapshot();
        let (_, stats) = snap
            .iter()
            .find(|(n, _)| n == "task.todo_typooo")
            .expect("typo counter must exist");
        assert_eq!(stats.unknown_method, 1);
        assert!(stats.last_error_at.is_some());
    }

    #[tokio::test]
    async fn capability_stats_snapshot_returns_sorted() {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "any"
            method = "anything.at.all"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &org_root, 3600).unwrap();
        // Send in reverse-alpha order; snapshot should sort.
        for m in ["zzz.method", "aaa.method", "mmm.method"] {
            let envelope = build_request(m, b"".to_vec(), bundle.clone(), 30);
            let _ = bridge.handle_inbound(envelope).await;
        }
        let snap = bridge.capability_stats_snapshot();
        let names: Vec<&str> = snap.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["aaa.method", "mmm.method", "zzz.method"]);
    }
}
