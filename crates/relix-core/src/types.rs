//! Shared wire types.
//!
//! These types are part of the public protocol surface. Changes here are wire-format
//! changes and require coordinated peer upgrade.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A node identity — the BLAKE3-256 hash of the node's Ed25519 public key.
///
/// This is the alpha equivalent of libp2p `PeerId` carried in our own wire envelope.
/// At Gate 2 we adopt libp2p `PeerId` directly; for the alpha we keep our own
/// type to avoid the libp2p dep in `relix-core`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(#[serde(with = "serde_bytes")] pub [u8; 32]);

impl NodeId {
    /// Construct from a public key's BLAKE3-256 hash.
    pub fn from_pubkey(pubkey: &[u8]) -> Self {
        let mut out = [0u8; 32];
        out.copy_from_slice(blake3::hash(pubkey).as_bytes());
        Self(out)
    }

    /// Hex-encoded short prefix for logs (8 chars).
    pub fn short(&self) -> String {
        hex::encode(&self.0[..4])
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", hex::encode(self.0))
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

/// Request ID — 16 random bytes per RELIX-1 §1.4.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(#[serde(with = "serde_bytes")] pub [u8; 16]);

impl RequestId {
    /// Generate a fresh random request ID.
    pub fn new() -> Self {
        use rand::RngCore;
        let mut out = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut out);
        Self(out)
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "rid:{}", hex::encode(self.0))
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

/// Distributed trace ID per RELIX-1 §1.11 (16 random bytes).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TraceId(#[serde(with = "serde_bytes")] pub [u8; 16]);

impl TraceId {
    /// Generate a fresh trace ID.
    pub fn new() -> Self {
        use rand::RngCore;
        let mut out = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut out);
        Self(out)
    }
}

impl Default for TraceId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tid:{}", hex::encode(self.0))
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

/// Flow ID — 16 random bytes per RELIX-8 §8.4.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FlowId(#[serde(with = "serde_bytes")] pub [u8; 16]);

impl FlowId {
    /// Generate a fresh flow ID.
    pub fn new() -> Self {
        use rand::RngCore;
        let mut out = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut out);
        Self(out)
    }
}

impl Default for FlowId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for FlowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "flow:{}", hex::encode(self.0))
    }
}

impl fmt::Display for FlowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", hex::encode(self.0))
    }
}

/// TAI-equivalent timestamp in seconds since Unix epoch. CBOR tag 1 on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// Current wall-clock time. NOT for use inside SOL flows — SOL uses the
    /// deterministic `Time.now()` capability (RELIX-7 §7.11). This is fine for
    /// audit and bundle issuance timestamps.
    pub fn now() -> Self {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Self(secs)
    }

    /// Add a duration in seconds.
    pub fn add_secs(self, secs: i64) -> Self {
        Self(self.0 + secs)
    }
}

/// Error envelope returned by `/relix/rpc/1` per RELIX-1 §1.6.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    /// Stable error kind (u16 per spec; widened to u32 for forward compat).
    pub kind: u32,
    /// Human-readable cause suitable for logs.
    pub cause: String,
    /// Retry hint: 0=retry_now, 1=retry_backoff, 2=do_not_retry, 3=retry_after.
    pub retry_hint: u8,
    /// Retry-after seconds, present iff retry_hint = 3.
    pub retry_after: Option<u32>,
}

/// Stable error-kind enumeration per RELIX-1 §1.6.
#[allow(missing_docs)]
pub mod error_kinds {
    pub const TRANSPORT: u32 = 1;
    pub const TIMEOUT: u32 = 2;
    pub const PEER_UNREACHABLE: u32 = 3;
    pub const UNKNOWN_METHOD: u32 = 4;
    pub const INVALID_ARGS: u32 = 5;
    pub const POLICY_DENIED: u32 = 6;
    pub const IDENTITY_INVALID: u32 = 7;
    pub const CREDENTIAL_EXPIRED: u32 = 8;
    pub const CAPABILITY_DEPRECATED: u32 = 9;
    pub const CAPABILITY_REMOVED: u32 = 10;
    pub const RESPONDER_INTERNAL: u32 = 11;
    pub const RESPONDER_OVERLOADED: u32 = 12;
    pub const REPLAY_REJECTED: u32 = 13;
    pub const VERSION_MISMATCH: u32 = 14;
    pub const APPROVAL_TIMEOUT: u32 = 15;
    pub const APPROVAL_DENIED: u32 = 16;
    pub const CANCELLED: u32 = 17;
    pub const MANIFEST_STALE: u32 = 18;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip_via_cbor() {
        let nid = NodeId::from_pubkey(b"test-pubkey");
        let bytes = crate::codec::encode(&nid).expect("encode");
        let back: NodeId = crate::codec::decode(&bytes).expect("decode");
        assert_eq!(nid, back);
    }

    #[test]
    fn request_ids_are_unique() {
        let a = RequestId::new();
        let b = RequestId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn timestamp_addition() {
        let t = Timestamp(1000);
        assert_eq!(t.add_secs(5).0, 1005);
    }

    #[test]
    fn error_envelope_roundtrip() {
        let e = ErrorEnvelope {
            kind: error_kinds::POLICY_DENIED,
            cause: "no matching allow rule".into(),
            retry_hint: 2,
            retry_after: None,
        };
        let bytes = crate::codec::encode(&e).expect("encode");
        let back: ErrorEnvelope = crate::codec::decode(&bytes).expect("decode");
        assert_eq!(e.kind, back.kind);
        assert_eq!(e.cause, back.cause);
    }
}
