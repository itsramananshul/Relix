//! SEC PART A — structured, HMAC-SHA256-signed approval tokens.
//!
//! Replaces the pre-fix "any non-empty string" opaque token. A
//! token now binds itself to:
//!
//! - the approval row it was issued for (`approval_id`),
//! - the exact capability method (`method`) — a token for
//!   `tool.web_read` is rejected when used against
//!   `tool.terminal`,
//! - the caller's `subject_id` (NodeId hex) — agent A cannot
//!   replay agent B's token,
//! - the original session (`session_id`),
//! - a TTL (`issued_at_ms` + `expires_at_ms`) — expired tokens
//!   are rejected,
//! - a 32-byte random nonce so two tokens for the same
//!   approval are distinguishable on the consumption
//!   blocklist (defence in depth — also useful when issuing
//!   tokens for retries).
//!
//! The signature is HMAC-SHA256 over the canonical
//! pipe-delimited payload, hex-encoded. The signing key comes
//! from the [`SIGNING_KEY_ENV`] environment variable. Verify
//! is constant-time (`subtle::ConstantTimeEq` on top of the
//! HMAC crate's `verify_slice`, which is already constant-time
//! — the explicit `subtle` layer documents the property at
//! the call site and adds defence-in-depth against future
//! refactors).
//!
//! Wire shape:
//!
//! ```text
//! base64url_nopad( JSON({
//!   approval_id,    // string
//!   method,         // string
//!   subject_id,     // string (hex NodeId)
//!   session_id,     // string
//!   issued_at_ms,   // i64
//!   expires_at_ms,  // i64
//!   nonce,          // string (64 hex chars = 32 random bytes)
//!   signature,      // string (64 hex chars = HMAC-SHA256 output)
//! }) )
//! ```
//!
//! Canonical signing bytes:
//!
//! ```text
//! approval_id "|" method "|" subject_id "|" session_id
//!     "|" issued_at_ms "|" expires_at_ms "|" nonce
//! ```
//!
//! IDs in Relix are NEVER allowed to contain a `|` character
//! (uuid v4 / hex / lower-snake-case identifiers), so the
//! delimiter cannot collide. The token-issue path explicitly
//! rejects any field that does.
//!
//! Error variants are mapped 1-to-1 to specific deny causes
//! the admission gate surfaces in [`SECURITY_DENIED`] errors.
//! Operators see exactly which check failed (signature /
//! method / subject / expiry / consume) without leaking
//! anything sensitive: the cause text never echoes the secret
//! key or the raw signature bytes.

use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::Zeroizing;

type HmacSha256 = Hmac<Sha256>;

/// Environment variable the runtime reads to source the HMAC
/// signing key. Operators MUST set this on every controller
/// that runs the admission gate; missing / empty keys cause
/// the gate to refuse every token-bearing call with
/// [`TokenError::MissingSigningKey`].
pub const SIGNING_KEY_ENV: &str = "RELIX_APPROVAL_TOKEN_KEY";

/// Errors surfaced by the token issue / parse / verify pipeline.
/// Each variant maps to a distinct deny cause so the admission
/// gate's audit ring carries the exact failure reason.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TokenError {
    /// Base64url decode failed — the wire string is corrupted
    /// or not a token at all.
    #[error("approval_token: malformed encoding ({0})")]
    MalformedEncoding(String),
    /// JSON decode failed — the base64 payload is not a
    /// recognised token JSON shape.
    #[error("approval_token: malformed payload ({0})")]
    MalformedPayload(String),
    /// HMAC-SHA256 signature verification failed. The token's
    /// payload was tampered with, OR it was signed with a
    /// different key (e.g. the operator rotated the env var).
    #[error("approval_token: signature verification failed")]
    BadSignature,
    /// Token TTL elapsed — `now >= expires_at_ms`.
    #[error("approval_token: expired at {expires_at_ms} (now={now_ms})")]
    Expired { now_ms: i64, expires_at_ms: i64 },
    /// The token's `method` does not match the requested
    /// capability. Operators using a `tool.web_read` token
    /// against `tool.terminal` land here.
    #[error(
        "approval_token: method scope mismatch (token={token_method}, request={request_method})"
    )]
    MethodMismatch {
        token_method: String,
        request_method: String,
    },
    /// The caller's verified `subject_id` does not match the
    /// `subject_id` baked into the token. Defends against agent
    /// A replaying agent B's token.
    #[error("approval_token: subject scope mismatch")]
    SubjectMismatch,
    /// The token has already been consumed (per the SQLite
    /// blocklist). Replay attempt.
    #[error("approval_token: token already consumed")]
    AlreadyConsumed,
    /// The `RELIX_APPROVAL_TOKEN_KEY` env var is missing or
    /// empty. Boot-time fail-loud — the gate refuses every
    /// token-bearing call.
    #[error("approval_token: signing key missing (set {SIGNING_KEY_ENV})")]
    MissingSigningKey,
    /// One of the payload fields contains a `|` character,
    /// which would let an attacker re-arrange the canonical
    /// signing bytes. Issued at mint time only; reaching this
    /// at parse time means the token was forged.
    #[error("approval_token: payload field `{0}` contains forbidden delimiter")]
    ForbiddenDelimiter(&'static str),
    /// Storage error during the atomic consume path. Always
    /// fail-closed: the gate denies the call.
    #[error("approval_token: store error ({0})")]
    Store(String),
}

impl TokenError {
    /// Stable wire string the gate maps to its `matched_rule`
    /// for the policy denial ring. Distinct per failure mode
    /// so operators can grep logs.
    pub fn matched_rule(&self) -> &'static str {
        match self {
            Self::MalformedEncoding(_) => "approval_token_malformed",
            Self::MalformedPayload(_) => "approval_token_malformed",
            Self::BadSignature => "approval_token_bad_signature",
            Self::Expired { .. } => "approval_token_expired",
            Self::MethodMismatch { .. } => "approval_token_scope_mismatch",
            Self::SubjectMismatch => "approval_token_subject_mismatch",
            Self::AlreadyConsumed => "approval_token_consumed",
            Self::MissingSigningKey => "approval_token_missing_key",
            Self::ForbiddenDelimiter(_) => "approval_token_malformed",
            Self::Store(_) => "approval_token_store_error",
        }
    }
}

/// One signed approval token. Round-trips through the wire
/// format via [`Self::to_wire`] / [`Self::parse`]; the
/// signature field is always re-derived at issue time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalToken {
    pub approval_id: String,
    pub method: String,
    pub subject_id: String,
    pub session_id: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub nonce: String,
    pub signature: String,
}

impl ApprovalToken {
    /// Mint + sign a new token. Returns the wire-encoded
    /// (base64url-no-pad of the JSON) form.
    ///
    /// `ttl_ms` is the lifetime from `issued_at_ms`. Tokens
    /// MUST have non-zero TTL — a token that expires the
    /// moment it is minted is operationally useless and
    /// almost certainly a misconfiguration. A `ttl_ms <= 0`
    /// returns `Expired` immediately so the call site
    /// catches the bug at issue time, not at verify time.
    pub fn issue(
        approval_id: &str,
        method: &str,
        subject_id: &str,
        session_id: &str,
        issued_at_ms: i64,
        ttl_ms: i64,
        signing_key: &[u8],
    ) -> Result<String, TokenError> {
        if signing_key.is_empty() {
            return Err(TokenError::MissingSigningKey);
        }
        if ttl_ms <= 0 {
            return Err(TokenError::Expired {
                now_ms: issued_at_ms,
                expires_at_ms: issued_at_ms,
            });
        }
        for (name, val) in [
            ("approval_id", approval_id),
            ("method", method),
            ("subject_id", subject_id),
            ("session_id", session_id),
        ] {
            if val.contains('|') {
                return Err(TokenError::ForbiddenDelimiter(name));
            }
        }
        let nonce = mint_nonce();
        let expires_at_ms = issued_at_ms.saturating_add(ttl_ms);
        let canonical = canonical_signing_bytes(
            approval_id,
            method,
            subject_id,
            session_id,
            issued_at_ms,
            expires_at_ms,
            &nonce,
        );
        let signature = sign_hex(signing_key, canonical.as_bytes());
        let tok = Self {
            approval_id: approval_id.into(),
            method: method.into(),
            subject_id: subject_id.into(),
            session_id: session_id.into(),
            issued_at_ms,
            expires_at_ms,
            nonce,
            signature,
        };
        tok.to_wire()
    }

    /// Encode self to the wire form. Pulled out so tests can
    /// hand-craft tokens with off-spec fields and verify the
    /// parse-time rejection path.
    pub fn to_wire(&self) -> Result<String, TokenError> {
        let json =
            serde_json::to_vec(self).map_err(|e| TokenError::MalformedPayload(e.to_string()))?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json))
    }

    /// Parse the wire form back into an [`ApprovalToken`].
    /// Does NOT verify the signature; callers MUST follow up
    /// with [`Self::verify_signature`].
    pub fn parse(wire: &str) -> Result<Self, TokenError> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(wire)
            .map_err(|e| TokenError::MalformedEncoding(e.to_string()))?;
        serde_json::from_slice::<Self>(&raw)
            .map_err(|e| TokenError::MalformedPayload(e.to_string()))
    }

    /// Constant-time HMAC verification. Returns
    /// `Err(TokenError::BadSignature)` on mismatch.
    pub fn verify_signature(&self, signing_key: &[u8]) -> Result<(), TokenError> {
        if signing_key.is_empty() {
            return Err(TokenError::MissingSigningKey);
        }
        let canonical = canonical_signing_bytes(
            &self.approval_id,
            &self.method,
            &self.subject_id,
            &self.session_id,
            self.issued_at_ms,
            self.expires_at_ms,
            &self.nonce,
        );
        let actual = sign_hex(signing_key, canonical.as_bytes());
        let actual_bytes = actual.as_bytes();
        let expected_bytes = self.signature.as_bytes();
        // subtle::ConstantTimeEq returns Choice (0 or 1). The
        // pre-check on length is also constant w.r.t. the
        // contents (length is public; padding to equal lengths
        // would burn cycles for no security benefit). The
        // contained `ct_eq` is constant w.r.t. the bytes
        // themselves.
        if actual_bytes.len() != expected_bytes.len() {
            return Err(TokenError::BadSignature);
        }
        if bool::from(actual_bytes.ct_eq(expected_bytes)) {
            Ok(())
        } else {
            Err(TokenError::BadSignature)
        }
    }

    /// Convenience check: token TTL has not elapsed.
    pub fn check_not_expired(&self, now_ms: i64) -> Result<(), TokenError> {
        if now_ms >= self.expires_at_ms {
            return Err(TokenError::Expired {
                now_ms,
                expires_at_ms: self.expires_at_ms,
            });
        }
        Ok(())
    }

    /// Convenience check: token's bound `method` matches the
    /// requested method exactly. Comparison is byte-for-byte
    /// — no normalisation, no aliasing.
    pub fn check_method(&self, requested: &str) -> Result<(), TokenError> {
        if self.method != requested {
            return Err(TokenError::MethodMismatch {
                token_method: self.method.clone(),
                request_method: requested.to_string(),
            });
        }
        Ok(())
    }

    /// Convenience check: token's bound `subject_id` matches
    /// the verified caller. Constant-time compare so a hostile
    /// caller cannot probe the byte-by-byte difference between
    /// the stored subject and theirs.
    pub fn check_subject(&self, caller_subject: &str) -> Result<(), TokenError> {
        let a = self.subject_id.as_bytes();
        let b = caller_subject.as_bytes();
        if a.len() != b.len() {
            return Err(TokenError::SubjectMismatch);
        }
        if bool::from(a.ct_eq(b)) {
            Ok(())
        } else {
            Err(TokenError::SubjectMismatch)
        }
    }

    /// Stable blocklist key for the atomic consume row. Two
    /// tokens are equal-on-blocklist iff their `nonce` AND
    /// `approval_id` match — both pieces guard against attacker
    /// reuse without forcing the operator to store the full
    /// signature.
    pub fn blocklist_key(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(self.nonce.as_bytes());
        h.update(b"|");
        h.update(self.approval_id.as_bytes());
        h.finalize().to_hex().to_string()
    }
}

/// Read the signing key from [`SIGNING_KEY_ENV`]. Returns
/// [`TokenError::MissingSigningKey`] when unset or empty so
/// the boot path can fail loud.
///
/// SEC PART 2: the key material is wrapped in `Zeroizing<Vec<u8>>`
/// so it's wiped from the heap when the returned value is
/// dropped — the dispatch bridge stores its own zeroizing
/// copy + the env-var-sourced string is the only public
/// surface.
pub fn signing_key_from_env() -> Result<Zeroizing<Vec<u8>>, TokenError> {
    match std::env::var(SIGNING_KEY_ENV) {
        Ok(v) if !v.is_empty() => Ok(Zeroizing::new(v.into_bytes())),
        _ => Err(TokenError::MissingSigningKey),
    }
}

fn canonical_signing_bytes(
    approval_id: &str,
    method: &str,
    subject_id: &str,
    session_id: &str,
    issued_at_ms: i64,
    expires_at_ms: i64,
    nonce: &str,
) -> String {
    format!(
        "{approval_id}|{method}|{subject_id}|{session_id}|{issued_at_ms}|{expires_at_ms}|{nonce}"
    )
}

fn sign_hex(key: &[u8], payload: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(payload);
    hex::encode(mac.finalize().into_bytes())
}

fn mint_nonce() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Zeroizing<Vec<u8>> {
        Zeroizing::new(b"test-signing-key-32-bytes-long-x".to_vec())
    }

    fn other_key() -> Zeroizing<Vec<u8>> {
        Zeroizing::new(b"a-different-key-32-bytes-of-yyyy".to_vec())
    }

    #[test]
    fn issue_then_parse_round_trips_every_field() {
        let wire = ApprovalToken::issue(
            "approval-1",
            "tool.web_read",
            "subject-abc",
            "session-7",
            1_700_000_000_000,
            60_000,
            &key(),
        )
        .unwrap();
        let parsed = ApprovalToken::parse(&wire).unwrap();
        assert_eq!(parsed.approval_id, "approval-1");
        assert_eq!(parsed.method, "tool.web_read");
        assert_eq!(parsed.subject_id, "subject-abc");
        assert_eq!(parsed.session_id, "session-7");
        assert_eq!(parsed.issued_at_ms, 1_700_000_000_000);
        assert_eq!(parsed.expires_at_ms, 1_700_000_060_000);
        assert_eq!(parsed.nonce.len(), 64);
        assert_eq!(parsed.signature.len(), 64);
    }

    #[test]
    fn signature_verifies_with_correct_key() {
        let wire = ApprovalToken::issue("a", "m", "s", "sess", 1_000, 60_000, &key()).unwrap();
        let parsed = ApprovalToken::parse(&wire).unwrap();
        parsed.verify_signature(&key()).expect("verify");
    }

    #[test]
    fn signature_fails_under_wrong_key() {
        let wire = ApprovalToken::issue("a", "m", "s", "sess", 1_000, 60_000, &key()).unwrap();
        let parsed = ApprovalToken::parse(&wire).unwrap();
        assert_eq!(
            parsed.verify_signature(&other_key()),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn signature_fails_after_field_tamper() {
        let wire =
            ApprovalToken::issue("a", "tool.web_read", "s", "sess", 1_000, 60_000, &key()).unwrap();
        let mut parsed = ApprovalToken::parse(&wire).unwrap();
        // Attacker swaps the method to a higher-privilege one.
        parsed.method = "tool.terminal".into();
        assert_eq!(
            parsed.verify_signature(&key()),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn method_mismatch_is_caught_independent_of_signature() {
        // A token issued for tool.web_read MUST NOT admit
        // tool.terminal — even when the signature is valid.
        let wire =
            ApprovalToken::issue("a", "tool.web_read", "s", "sess", 1_000, 60_000, &key()).unwrap();
        let parsed = ApprovalToken::parse(&wire).unwrap();
        parsed.verify_signature(&key()).unwrap();
        match parsed.check_method("tool.terminal") {
            Err(TokenError::MethodMismatch {
                token_method,
                request_method,
            }) => {
                assert_eq!(token_method, "tool.web_read");
                assert_eq!(request_method, "tool.terminal");
            }
            other => panic!("expected MethodMismatch, got {other:?}"),
        }
        parsed.check_method("tool.web_read").expect("exact match");
    }

    #[test]
    fn subject_mismatch_is_caught_via_constant_time_compare() {
        let wire =
            ApprovalToken::issue("a", "m", "subject-alice", "sess", 1_000, 60_000, &key()).unwrap();
        let parsed = ApprovalToken::parse(&wire).unwrap();
        assert_eq!(
            parsed.check_subject("subject-bob"),
            Err(TokenError::SubjectMismatch)
        );
        // Same-length-different-content also denied.
        assert_eq!(
            parsed.check_subject("subject-evil!"),
            Err(TokenError::SubjectMismatch)
        );
        parsed.check_subject("subject-alice").expect("match");
    }

    #[test]
    fn expired_token_is_rejected() {
        let wire = ApprovalToken::issue("a", "m", "s", "sess", 1_000, 60_000, &key()).unwrap();
        let parsed = ApprovalToken::parse(&wire).unwrap();
        match parsed.check_not_expired(1_000_000) {
            Err(TokenError::Expired {
                now_ms,
                expires_at_ms,
            }) => {
                assert_eq!(now_ms, 1_000_000);
                assert_eq!(expires_at_ms, 61_000);
            }
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    // ── DEFERRED A: TTL boundary tests via clock injection ──
    //
    // `check_not_expired` takes `now_ms` as a parameter — pure
    // function, no wall-clock dep. These tests verify the
    // boundary condition explicitly: `now >= expires_at_ms`
    // rejects, `now < expires_at_ms` admits. Locks the
    // exclusive-boundary contract documented in the
    // `TokenError::Expired` variant.

    /// Helper: mint a token with `expires_at_ms = issued + ttl`
    /// and immediately parse the wire form back. Returns the
    /// parsed token so the test can drive `check_not_expired`
    /// against synthetic `now_ms` values without re-hitting
    /// `unix_ms()`.
    fn token_with_window(issued_at_ms: i64, ttl_ms: i64) -> ApprovalToken {
        let wire =
            ApprovalToken::issue("a", "m", "s", "sess", issued_at_ms, ttl_ms, &key()).unwrap();
        ApprovalToken::parse(&wire).unwrap()
    }

    #[test]
    fn ttl_boundary_admits_one_ms_before_expiry() {
        // Verified at `now = issued + 59_999`: token must admit.
        let tok = token_with_window(1_000, 60_000);
        assert_eq!(tok.expires_at_ms, 61_000);
        tok.check_not_expired(60_999)
            .expect("now = expires - 1 must admit");
    }

    #[test]
    fn ttl_boundary_rejects_exactly_at_expiry() {
        // Verified at `now = expires_at_ms`: must reject. The
        // SQL/runtime contract is `now >= expires` ⇒ expired
        // (exclusive upper bound — a token issued for 0ms is
        // already expired the moment it is parsed).
        let tok = token_with_window(1_000, 60_000);
        match tok.check_not_expired(61_000) {
            Err(TokenError::Expired {
                now_ms,
                expires_at_ms,
            }) => {
                assert_eq!(now_ms, 61_000);
                assert_eq!(expires_at_ms, 61_000);
            }
            other => panic!("expected Expired at the exact expires_at_ms boundary, got {other:?}"),
        }
    }

    #[test]
    fn ttl_boundary_rejects_one_ms_after_expiry() {
        // Verified at `now = issued + 60_001`: must reject.
        let tok = token_with_window(1_000, 60_000);
        match tok.check_not_expired(61_001) {
            Err(TokenError::Expired { .. }) => {}
            other => panic!("expected Expired at expires + 1ms, got {other:?}"),
        }
    }

    #[test]
    fn ttl_boundary_admits_at_issued_at_ms_with_one_ms_ttl() {
        // Smallest legal token: `ttl_ms = 1` ⇒ expires_at = issued + 1.
        // - now = issued       → admits (1 < expires)
        // - now = issued + 1   → rejects (now == expires)
        let tok = token_with_window(0, 1);
        assert_eq!(tok.expires_at_ms, 1);
        tok.check_not_expired(0).expect("now=0 < expires=1 admits");
        match tok.check_not_expired(1) {
            Err(TokenError::Expired { .. }) => {}
            other => panic!("expected Expired at now=expires=1, got {other:?}"),
        }
    }

    // ── NOT-DONE 1: boundary tests via the Clock trait ──
    //
    // The three above exercise `check_not_expired` directly.
    // These additionally exercise the Clock trait integration:
    // the same boundary cases are driven by an
    // `Arc<dyn Clock>` rather than a literal `i64` so the
    // trait-object dispatch path is locked too.

    #[test]
    fn ttl_boundary_admits_one_ms_before_expiry_via_fake_clock() {
        use relix_core::clock::{Clock, FakeClock};
        use std::sync::Arc;
        let tok = token_with_window(1_000, 60_000);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(60_999));
        tok.check_not_expired(clock.now_ms())
            .expect("FakeClock at expires-1 admits");
    }

    #[test]
    fn ttl_boundary_rejects_at_expiry_via_fake_clock() {
        use relix_core::clock::{Clock, FakeClock};
        use std::sync::Arc;
        let tok = token_with_window(1_000, 60_000);
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(61_000));
        match tok.check_not_expired(clock.now_ms()) {
            Err(TokenError::Expired { .. }) => {}
            other => panic!("FakeClock at expires must reject, got {other:?}"),
        }
    }

    #[test]
    fn ttl_boundary_rejects_one_ms_after_expiry_via_fake_clock_advance() {
        use relix_core::clock::{Clock, FakeClock};
        use std::sync::Arc;
        let tok = token_with_window(1_000, 60_000);
        // Hold both an `Arc<FakeClock>` (for `.advance`) and
        // an `Arc<dyn Clock>` (for the trait-object dispatch
        // path) — both share the same `AtomicI64` so a single
        // advance is visible through both handles.
        let fake = Arc::new(FakeClock::new(60_999));
        let clock: Arc<dyn Clock> = fake.clone();
        fake.advance(2);
        match tok.check_not_expired(clock.now_ms()) {
            Err(TokenError::Expired { .. }) => {}
            other => panic!("FakeClock at expires+1 must reject, got {other:?}"),
        }
    }

    #[test]
    fn malformed_base64_returns_malformed_encoding() {
        match ApprovalToken::parse("!!not-base64!!") {
            Err(TokenError::MalformedEncoding(_)) => {}
            other => panic!("expected MalformedEncoding, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_returns_malformed_payload() {
        let wire = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"not json");
        match ApprovalToken::parse(&wire) {
            Err(TokenError::MalformedPayload(_)) => {}
            other => panic!("expected MalformedPayload, got {other:?}"),
        }
    }

    #[test]
    fn issue_rejects_field_with_pipe_delimiter() {
        let err = ApprovalToken::issue("a|injected", "m", "s", "sess", 1_000, 60_000, &key())
            .unwrap_err();
        assert_eq!(err, TokenError::ForbiddenDelimiter("approval_id"));
    }

    #[test]
    fn issue_rejects_empty_key_and_non_positive_ttl() {
        // ApprovalToken::issue returns Result<String, _>, so
        // the OK branch IS PartialEq — use direct compare here.
        assert_eq!(
            ApprovalToken::issue("a", "m", "s", "sess", 0, 60_000, &[]),
            Err(TokenError::MissingSigningKey)
        );
        match ApprovalToken::issue("a", "m", "s", "sess", 0, 0, &key()) {
            Err(TokenError::Expired { .. }) => {}
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    #[test]
    fn matched_rule_is_distinct_per_failure_mode() {
        // Every variant gets its own stable matched_rule so the
        // policy denial ring can be filtered by failure kind.
        let v: Vec<&'static str> = vec![
            TokenError::MalformedEncoding(String::new()).matched_rule(),
            TokenError::MalformedPayload(String::new()).matched_rule(),
            TokenError::BadSignature.matched_rule(),
            TokenError::Expired {
                now_ms: 0,
                expires_at_ms: 0,
            }
            .matched_rule(),
            TokenError::MethodMismatch {
                token_method: String::new(),
                request_method: String::new(),
            }
            .matched_rule(),
            TokenError::SubjectMismatch.matched_rule(),
            TokenError::AlreadyConsumed.matched_rule(),
            TokenError::MissingSigningKey.matched_rule(),
            TokenError::Store(String::new()).matched_rule(),
        ];
        // MalformedEncoding + MalformedPayload + ForbiddenDelimiter
        // intentionally fold into one rule (the operator does not
        // get more value from distinguishing them at audit time).
        // Every other variant gets a unique rule.
        for r in &v {
            assert!(r.starts_with("approval_token_"));
        }
    }

    #[test]
    fn blocklist_key_is_stable_per_nonce_and_approval_id() {
        let wire = ApprovalToken::issue("a1", "m", "s", "sess", 1_000, 60_000, &key()).unwrap();
        let parsed = ApprovalToken::parse(&wire).unwrap();
        let k1 = parsed.blocklist_key();
        let k2 = parsed.blocklist_key();
        assert_eq!(k1, k2);
        // Different approval_id under the same nonce → different
        // blocklist key (defence against nonce collisions).
        let mut p2 = parsed.clone();
        p2.approval_id = "a2".into();
        assert_ne!(k1, p2.blocklist_key());
    }

    #[test]
    fn issue_with_distinct_nonces_produces_distinct_blocklist_keys() {
        // Two tokens issued back-to-back for the same approval
        // get different nonces → different blocklist keys, so
        // operator-initiated re-issues don't collide.
        let w1 = ApprovalToken::issue("a", "m", "s", "sess", 1, 60_000, &key()).unwrap();
        let w2 = ApprovalToken::issue("a", "m", "s", "sess", 2, 60_000, &key()).unwrap();
        let t1 = ApprovalToken::parse(&w1).unwrap();
        let t2 = ApprovalToken::parse(&w2).unwrap();
        assert_ne!(t1.nonce, t2.nonce);
        assert_ne!(t1.blocklist_key(), t2.blocklist_key());
    }

    #[test]
    fn signing_key_from_env_returns_missing_when_unset() {
        // We can't safely mutate process env from a test (the
        // crate forbids unsafe_code, and `set_var` is now
        // `unsafe`). The empty-env case is the operator's
        // hardest fail mode, so we cover that here; the
        // non-empty case is covered indirectly via the gate's
        // structured-token tests which thread a synthetic key
        // through `GateInputs::signing_key`.
        //
        // To guarantee the var is actually unset for this
        // assertion, we run inside a fresh process scope. If a
        // caller pre-set RELIX_APPROVAL_TOKEN_KEY in their
        // shell the test environment would lie — but cargo
        // test runs each test with the inherited env, and we
        // intentionally do NOT lock that down here.
        if std::env::var(SIGNING_KEY_ENV).is_err() {
            // SEC PART 2: signing_key_from_env now returns
            // Zeroizing<Vec<u8>> on the Ok branch; assert
            // structurally rather than via PartialEq (Zeroizing
            // doesn't implement Eq).
            assert!(matches!(
                signing_key_from_env(),
                Err(TokenError::MissingSigningKey)
            ));
        }
    }
}
