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
        })
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
        let outcome = handler.invoke(ctx).await;

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
}
