//! RELIX-1 request/response envelope — alpha subset.
//!
//! Fields chosen per `specs/RELIX-1-rpc.md` §1.4 / §1.5; alpha SIMPs:
//! - Signed-envelope (`sig`) deferred — no capability requires it in the alpha.
//! - Attenuated-token (`at`) deferred — no on-behalf-of chains yet.
//! - Idempotency cache deferred (capabilities are alpha-idempotent by design).

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

use relix_core::bundle::Bundle;
use relix_core::types::{ErrorEnvelope, NodeId, RequestId, Timestamp, TraceId};

/// RELIX-1 request envelope (alpha fields).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestEnvelope {
    /// Protocol version. Currently 1.
    pub pv: u8,
    /// Request ID — 16 random bytes (RELIX-1 §1.4 `rid`).
    pub rid: RequestId,
    /// Trace ID (`tid`).
    pub tid: TraceId,
    /// Fully-qualified method name.
    pub method: String,
    /// Pinned capability major version.
    pub mv: u32,
    /// Application-level arguments (CBOR; type per capability descriptor).
    pub args: ByteBuf,
    /// Caller's signed IdentityBundle (RELIX-1 §1.4 `ib`).
    pub identity_bundle: Bundle,
    /// Absolute deadline (`dl`) — unix seconds.
    pub deadline: Timestamp,
    /// P2 — RELIX-1 §1.7 freshness anchor. Unix milliseconds
    /// stamped by the issuer when the envelope is built. The
    /// responder rejects envelopes whose
    /// `|now_ms - issued_at_ms| > max_clock_skew_ms`
    /// (`STALE_ENVELOPE`) so a captured envelope cannot be
    /// replayed past the freshness window. Older clients that
    /// omit the field surface as `issued_at_ms = 0`, which
    /// always fails the freshness check on a modern responder.
    #[serde(default)]
    pub issued_at_ms: i64,
    /// Surface tag identifying where the call originated.
    /// Operator-asserted (not cryptographically proven). Used
    /// by the agent-employee admission gate to enforce
    /// `surface_allowlist`. `None` is treated as "unknown
    /// surface" and admitted only when the agent has an
    /// empty surface_allowlist. Additive on the wire (defaults
    /// to None on older clients).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    /// One-shot approval token from a prior
    /// `coord.approval.decide`. When present, the agent gate
    /// looks it up and admits the call if (a) the token is
    /// approved + unconsumed, (b) the method matches the
    /// approval record. Consumed on first successful admit.
    /// Additive — older clients send `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_token: Option<String>,
    /// Optional coordinator task_id the caller is acting on
    /// behalf of. When the agent gate fires `RequireApproval`
    /// the coordinator stamps this id on the new
    /// `approval_requests` row, flips the task to
    /// `awaiting_input`, and resumes it on
    /// `coord.approval.decide`. Additive — older clients send
    /// `None` and the approval still rounds-trips through
    /// poll/decide, just without auto-pausing a calling task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Optional logical session id the caller is acting inside.
    /// Standing approvals can bind to this so an operator can
    /// approve a session window rather than one call at a time.
    /// Additive: older clients omit it and session-scoped
    /// approvals simply do not match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Optional workspace path for the execution context. This
    /// lets workspace/path-scoped standing approvals match the
    /// actual run location instead of trusting a capability's raw
    /// argument string. Additive: older clients omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    /// GAP 23: per-request tenant identifier. Operator-asserted
    /// (not cryptographically proven). The bridge stamps it
    /// from the `X-Relix-Tenant` header before issuing a mesh
    /// dispatch; mesh-internal callers may pass it explicitly.
    /// `None` is treated as the default tenant — every
    /// downstream cap that consults the tenant defaults to
    /// `"default"` when this field is absent, so the wire
    /// format stays backwards-compatible with older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    /// P5 — wire-encoded session token (per
    /// [`crate::identity::SessionToken`]). When
    /// `[identity.session] verify_on_dispatch = true` AND the
    /// responder has a `SessionIdentityService` wired,
    /// admission step 6 verifies this token before invoking
    /// the handler. The token's signed `scopes` MUST include
    /// the requested capability method or admission denies
    /// with `SECURITY_DENIED token_insufficient_scope`. Older
    /// clients that omit the field surface as `None`;
    /// admission rejects with `SECURITY_DENIED
    /// session_token_missing` when verification is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
}

/// RELIX-1 response envelope (alpha fields).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    /// Protocol version (must match request).
    pub pv: u8,
    /// Echoed request id.
    pub rid: RequestId,
    /// Responder node id.
    pub responder: NodeId,
    /// Outcome.
    pub res: ResponseResult,
    /// Audit record id (16 bytes hex-printable) — for cross-correlation.
    pub aid: ByteBuf,
    /// Processed-at timestamp.
    pub processed_at: Timestamp,
    /// RELIX-7.19: optional per-call confidence score stamped
    /// by the responder's `ConfidenceScorer`. `None` when the
    /// responder didn't wire confidence scoring — older clients
    /// + responders coexist on the wire unchanged.
    ///   Caller-side
    ///   dispatch reads this to update the SOL `last_confidence()`
    ///   cell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

/// Outcome of an RPC.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseResult {
    /// Success — type per capability.
    Ok(ByteBuf),
    /// Error envelope per RELIX-1 §1.6.
    Err(ErrorEnvelope),
    /// SIMP: streaming over unary not modeled here; AI streaming uses a
    /// separate RELIX-2 substream protocol (`relix-runtime::transport::stream`).
    /// Capabilities marked `stream_out` use that path; their unary call site
    /// returns `Ok(stream_token)` where the body is delivered out-of-band.
    StreamHandle(ByteBuf),
}
