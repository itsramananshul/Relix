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
    capability_stats: std::sync::RwLock<HashMap<String, CapStats>>,
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
}

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
            capability_stats: std::sync::RwLock::new(HashMap::new()),
        })
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
    let req = RequestEnvelope {
        pv: 1,
        rid: relix_core::types::RequestId::new(),
        tid: relix_core::types::TraceId::new(),
        method: method.into(),
        mv: 1,
        args: ByteBuf::from(args),
        identity_bundle: identity,
        deadline: Timestamp::now().add_secs(deadline_secs_from_now),
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
