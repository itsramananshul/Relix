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
