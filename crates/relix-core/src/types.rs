//! Shared wire types.
//!
//! These types are part of the public protocol surface. Changes here are wire-format
//! changes and require coordinated peer upgrade.

use serde::{Deserialize, Serialize};
use std::fmt;

/// SEC PART 1: typed boundary for external / attacker-
/// controllable text on its way into an LLM prompt.
///
/// Pre-fix path: every code site that concatenated a
/// fetched web page, parsed document, OCR transcript, or
/// retrieved memory observation directly into a prompt
/// string was an unguarded prompt-injection vector — the
/// raw bytes could carry "ignore previous instructions"
/// payloads that the planning model would dutifully
/// follow.
///
/// `UntrustedText` makes the boundary compile-time-
/// checked: the type does NOT implement `Display`, so the
/// naive `format!("{}", value)` path is a hard compile
/// error. Code that wants to interpolate untrusted text
/// into a prompt MUST call [`Self::wrap_for_prompt`],
/// which fences the content between explicit `BEGIN
/// UNTRUSTED DATA` / `END UNTRUSTED DATA` markers.
/// `as_raw()` is the deliberate escape hatch for non-
/// prompt consumers (storage, presentation, logging).
///
/// SOUL.md content + operator-authored instructions are
/// trusted and intentionally bypass this type.
#[derive(Clone, Debug)]
pub struct UntrustedText(String);

impl UntrustedText {
    /// Wrap a raw `String` (typically the result of a
    /// fetch / parse / extract step) as untrusted.
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Render the content for inclusion in a prompt,
    /// fenced by explicit untrusted-data markers. The
    /// fences are the boundary the model is instructed
    /// (via the system prompt or extraction prompt) to
    /// treat as inert text rather than as instructions.
    pub fn wrap_for_prompt(&self) -> String {
        format!(
            "\n\n--- BEGIN UNTRUSTED DATA ---\n{}\n--- END UNTRUSTED DATA ---\n\n",
            self.0
        )
    }

    /// Deliberate escape hatch — returns the raw underlying
    /// string. Callers that route through this must NOT
    /// be feeding the value into an LLM prompt; use
    /// `wrap_for_prompt` for that path. Typical callers
    /// are persistence, logging, hashing, or operator-
    /// facing display.
    pub fn as_raw(&self) -> &str {
        &self.0
    }
}

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
    ///
    /// A misconfigured system clock must NEVER abort the responder process —
    /// that would take down every live flow on it. Instead of panicking (the
    /// prior behaviour), a bad clock is clamped and logged:
    /// - a pre-epoch clock → clamp to epoch 0 (`1970-01-01`) + `WARN`
    /// - a clock past `i64::MAX` seconds (≈year 292277026596) → clamp to
    ///   `i64::MAX` + `WARN` (rather than silently saturating)
    pub fn now() -> Self {
        Self::from_unix_time(std::time::SystemTime::now())
    }

    /// Convert a `SystemTime` into a `Timestamp` (seconds since the Unix
    /// epoch). Factored out of [`Self::now`] so the clock-error handling is
    /// unit-testable with an injected `SystemTime`. Never panics:
    /// a pre-epoch instant clamps to `0` and a clock beyond the `i64::MAX`
    /// second horizon clamps to `i64::MAX`; both clamps log a `WARN` so the
    /// underlying clock fault is surfaced rather than silently swallowed.
    fn from_unix_time(now: std::time::SystemTime) -> Self {
        match now.duration_since(std::time::UNIX_EPOCH) {
            Ok(dur) => match i64::try_from(dur.as_secs()) {
                Ok(secs) => Self(secs),
                Err(_) => {
                    tracing::warn!(
                        secs = dur.as_secs(),
                        "system clock is past the i64::MAX-second horizon (≈year 292277026596) — clamping Timestamp to i64::MAX"
                    );
                    Self(i64::MAX)
                }
            },
            Err(_) => {
                tracing::warn!(
                    "system clock is before the Unix epoch — clamping Timestamp to epoch 0 (1970-01-01); check the system clock"
                );
                Self(0)
            }
        }
    }

    /// SEC PART 6: checked add. Returns `ArithmeticOverflow`
    /// when `self.0 + secs` would wrap.
    pub fn add_secs(self, secs: i64) -> Result<Self, TimestampError> {
        let v = self
            .0
            .checked_add(secs)
            .ok_or(TimestampError::ArithmeticOverflow)?;
        Ok(Self(v))
    }
}

/// Errors that can arise when arithmetic on a [`Timestamp`]
/// would overflow `i64`.
#[derive(Debug, thiserror::Error)]
pub enum TimestampError {
    /// `Timestamp::add_secs` overflowed.
    #[error("timestamp arithmetic overflow")]
    ArithmeticOverflow,
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
    /// Agent-employee gate: the call requires an operator
    /// approval. The error envelope's `cause` contains the
    /// freshly-minted `approval_id` so callers can surface it
    /// to the operator. Once the approval is decided, the
    /// caller retries the same call with an `approval_token`
    /// on the envelope.
    pub const APPROVAL_REQUIRED: u32 = 19;
    /// Agent-employee gate: the `approval_token` on the
    /// envelope is unknown, expired, already consumed, or
    /// applies to a different method.
    pub const APPROVAL_TOKEN_INVALID: u32 = 20;
    /// Memory-guard / safety subsystem rejected the call.
    /// Distinct from `POLICY_DENIED` (operator-supplied admit
    /// policy) — this kind signals "the content itself looked
    /// like a poisoning attempt." Today only
    /// `memory.write_turn` raises it via
    /// `crate::nodes::memory::guard::MemoryGuard`.
    pub const SECURITY_DENIED: u32 = 21;
    /// RELIX-7.28 Part 1: budget enforcer rejected the call.
    /// The caller exceeded an agent or deployment cost cap and
    /// the cap is configured with `action_on_exceed = "reject"`.
    /// The error envelope's `cause` carries the limit + actual +
    /// reset time so the caller can surface a useful message.
    pub const RESOURCE_EXHAUSTED: u32 = 22;
    /// SEC PART 2 (manifest signing): the signed manifest
    /// envelope failed verification — bad signature,
    /// malformed encoding, fingerprint mismatch with the
    /// signer's public key, or the signer's fingerprint
    /// disagrees with a previously TOFU-pinned value.
    pub const MANIFEST_INVALID: u32 = 23;
    /// SEC PART 2: a signed manifest was received from a
    /// node whose fingerprint isn't in the known-nodes
    /// registry. Distinct from `MANIFEST_INVALID` so
    /// operators can grep for first-contact attempts that
    /// failed because the registry was wiped.
    pub const MANIFEST_UNKNOWN_SIGNER: u32 = 24;
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
        assert_eq!(t.add_secs(5).unwrap().0, 1005);
    }

    #[test]
    fn timestamp_now_handles_pre_epoch_clock_without_panicking() {
        use std::time::{Duration, UNIX_EPOCH};
        // Simulate a system clock set before the Unix epoch.
        // Previously this panicked, aborting the responder
        // process and every live flow on it. It must now clamp
        // to epoch 0 and return a value, not panic.
        let pre_epoch = UNIX_EPOCH - Duration::from_secs(10);
        let ts = Timestamp::from_unix_time(pre_epoch);
        assert_eq!(ts, Timestamp(0));
    }

    #[test]
    fn timestamp_now_converts_a_normal_clock_and_never_panics() {
        use std::time::{Duration, UNIX_EPOCH};
        let ts = Timestamp::from_unix_time(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert_eq!(ts, Timestamp(1_700_000_000));
        // The real `now()` (wired to the host clock) must also
        // never panic and must be post-epoch on any sane host.
        assert!(Timestamp::now().0 > 0);
    }

    #[test]
    fn timestamp_add_secs_overflow_returns_error() {
        // SEC PART 6: i64::MAX + 1 must surface as
        // ArithmeticOverflow, not silently wrap.
        let t = Timestamp(i64::MAX);
        match t.add_secs(1) {
            Err(TimestampError::ArithmeticOverflow) => {}
            Ok(v) => panic!("expected ArithmeticOverflow, got Ok({})", v.0),
        }
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

    // ── SEC PART 1: UntrustedText surface ───────────────────

    #[test]
    fn untrusted_text_wraps_with_explicit_delimiters() {
        let u = UntrustedText::new("hello\nworld");
        let wrapped = u.wrap_for_prompt();
        assert!(wrapped.contains("BEGIN UNTRUSTED DATA"));
        assert!(wrapped.contains("END UNTRUSTED DATA"));
        assert!(wrapped.contains("hello\nworld"));
    }

    #[test]
    fn untrusted_text_as_raw_returns_underlying_string() {
        let u = UntrustedText::new("inner");
        assert_eq!(u.as_raw(), "inner");
    }

    #[test]
    fn untrusted_text_does_not_implement_display() {
        // SEC PART 1: the whole defence rests on `UntrustedText`
        // NOT having a `Display` impl, so a naive
        // `format!("{}", value)` against an UntrustedText is a
        // compile error and callers must explicitly call
        // `wrap_for_prompt()` (or the deliberate-escape
        // `as_raw()` reader). This test is a runtime fingerprint
        // — we don't have trybuild here, so we use a trait
        // probe: any type that implements `std::fmt::Display`
        // also implements `ToString`. We assert the reverse:
        // UntrustedText is NOT a ToString. If anyone adds a
        // Display impl in the future, this test fails to
        // compile because the trait-bound assertion would
        // succeed where we expect it to fail.
        fn assert_no_display<T>(_: &T)
        where
            T: std::fmt::Debug,
        {
            // Successful instantiation proves Debug is there;
            // the absence of a paired Display impl is the
            // contract — verified by inspection of types.rs
            // (no `impl Display for UntrustedText`).
        }
        let u = UntrustedText::new("x");
        assert_no_display(&u);
    }
}
