//! Ed25519-signed approval tokens.
//!
//! Replaces the prior HMAC-SHA256 scheme. RELIX spec
//! `specs/identity-employees.md` §H.5 mandates: "Approver
//! signs `approval.granted{nonce, decision}` envelope.
//! Responding node verifies the approver satisfies the policy
//! criteria." HMAC is symmetric — anyone holding the key can
//! forge any token. Ed25519 is asymmetric — the signing key
//! stays private on the issuer; verifiers carry only the
//! verification (public) key.
//!
//! A token now binds itself to:
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
//!   blocklist,
//! - the verification-key fingerprint (`signing_key_fingerprint`)
//!   so verifiers can look up the correct public key in the
//!   trusted key set.
//!
//! The signature is Ed25519 over the canonical pipe-delimited
//! payload prefixed with the protocol version byte. Verification
//! uses `ed25519_dalek::VerifyingKey::verify_strict` so weak
//! signatures (e.g. small-order or non-canonical R / S values)
//! are rejected.
//!
//! ## Wire shape
//!
//! ```text
//! base64url_nopad( JSON({
//!   version,                  // u8, MUST be 0x02
//!   approval_id,              // string
//!   method,                   // string
//!   subject_id,               // string (hex NodeId)
//!   session_id,               // string
//!   issued_at_ms,             // i64
//!   expires_at_ms,            // i64
//!   nonce,                    // string (64 hex chars = 32 random bytes)
//!   signing_key_fingerprint,  // string (hex BLAKE3 prefix of the verifying key)
//!   signature,                // string (base64url-nopad Ed25519 signature)
//! }) )
//! ```
//!
//! ## Canonical signing bytes
//!
//! ```text
//! 0x02
//!     || approval_id "|" method "|" subject_id "|" session_id
//!     "|" issued_at_ms "|" expires_at_ms "|" nonce
//!     "|" signing_key_fingerprint
//! ```
//!
//! IDs in Relix are NEVER allowed to contain a `|` character
//! (uuid v4 / hex / lower-snake-case identifiers), so the
//! delimiter cannot collide. The token-issue path explicitly
//! rejects any field that does.
//!
//! ## Legacy migration
//!
//! Tokens missing the `version` field (or carrying `version =
//! 0x01`, the legacy HMAC-SHA256 wire) are rejected with
//! [`TokenError::TokenFormatDeprecated`] at parse time. The
//! admission gate surfaces this to operators as
//! `approval_token_format_deprecated` so the corrective
//! action — restart any agent still holding an HMAC token —
//! is visible in the policy denial ring.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

/// Environment variable the runtime reads to source the
/// Ed25519 signing-key seed. The value MUST be a 32-byte seed
/// encoded as 64 hex characters. Operators set this on every
/// controller that issues approval tokens.
pub const SIGNING_KEY_ENV: &str = "RELIX_APPROVAL_SIGNING_KEY";

/// Current canonical wire-format version byte.
pub const TOKEN_VERSION: u8 = 0x02;

/// Legacy HMAC-SHA256 wire-format version byte. Tokens carrying
/// this value (or no `version` field at all, which defaults to
/// `0x01` for the missing case) are rejected with
/// [`TokenError::TokenFormatDeprecated`].
pub const TOKEN_VERSION_LEGACY_HMAC: u8 = 0x01;

/// Number of hex characters used for the verification-key
/// fingerprint. 32 = 128 bits of BLAKE3 prefix — well over the
/// birthday bound even at trillions of operator-deployed keys.
const FINGERPRINT_HEX_LEN: usize = 32;

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
    /// Ed25519 signature verification failed. The token's
    /// payload was tampered with, OR it was signed with a
    /// different key.
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
    /// The signing-key env var is missing, empty, or
    /// malformed (not 64 hex characters).
    #[error(
        "approval_token: signing key missing or malformed (set {SIGNING_KEY_ENV} to a 64-hex-char Ed25519 seed)"
    )]
    MissingSigningKey,
    /// The verification-key fingerprint on the wire is not in
    /// the configured key set. Operators have not deployed
    /// the corresponding public key on this responder.
    #[error("approval_token: unknown signing key fingerprint `{0}`")]
    UnknownSigningKey(String),
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
    /// Legacy HMAC-SHA256 format detected. Operators must
    /// restart any agent still holding an old token; this
    /// implementation no longer accepts the HMAC scheme.
    #[error(
        "approval_token: deprecated HMAC token format (version={got:#04x}); \
         restart any agents holding old tokens — only Ed25519 (version={expected:#04x}) accepted"
    )]
    TokenFormatDeprecated { got: u8, expected: u8 },
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
            Self::UnknownSigningKey(_) => "approval_token_unknown_signer",
            Self::ForbiddenDelimiter(_) => "approval_token_malformed",
            Self::Store(_) => "approval_token_store_error",
            Self::TokenFormatDeprecated { .. } => "approval_token_format_deprecated",
        }
    }
}

/// Issuer-side handle: holds the Ed25519 signing key + the
/// precomputed verification-key fingerprint. Cheap to clone;
/// the underlying signing-key bytes are zeroized on drop via
/// `ed25519_dalek::SigningKey`'s `Zeroize` impl.
#[derive(Clone)]
pub struct ApprovalSigner {
    inner: Arc<ApprovalSignerInner>,
}

impl std::fmt::Debug for ApprovalSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the signing-key bytes. Surface only the
        // fingerprint so log lines + assertion-failure messages
        // identify the key without leaking it.
        f.debug_struct("ApprovalSigner")
            .field("fingerprint", &self.inner.fingerprint)
            .finish()
    }
}

struct ApprovalSignerInner {
    signing: SigningKey,
    verifying: VerifyingKey,
    fingerprint: String,
}

impl ApprovalSigner {
    /// Build a signer from a 32-byte Ed25519 seed. The seed is
    /// consumed (moved into the signer) and zeroized on drop.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(&seed);
        let verifying = signing.verifying_key();
        let fingerprint = compute_fingerprint(&verifying);
        // The seed array is on the caller's stack; `signing`
        // owns its own zeroizing copy. Wipe the caller's copy
        // here so a debugger snapshot post-call doesn't see it.
        let mut s = seed;
        s.zeroize();
        Self {
            inner: Arc::new(ApprovalSignerInner {
                signing,
                verifying,
                fingerprint,
            }),
        }
    }

    /// Build a signer from a hex-encoded 32-byte seed string.
    /// Returns `MissingSigningKey` when the input is not exactly
    /// 64 hex characters.
    pub fn from_hex_seed(hex_seed: &str) -> Result<Self, TokenError> {
        let trimmed = hex_seed.trim();
        if trimmed.len() != 64 {
            return Err(TokenError::MissingSigningKey);
        }
        let raw = hex::decode(trimmed).map_err(|_| TokenError::MissingSigningKey)?;
        if raw.len() != 32 {
            return Err(TokenError::MissingSigningKey);
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&raw);
        let s = Self::from_seed(arr);
        // Wipe the intermediate decoded vec too.
        let mut z = Zeroizing::new(raw);
        z.zeroize();
        Ok(s)
    }

    /// Build a signer by reading `RELIX_APPROVAL_SIGNING_KEY`
    /// from the environment. Returns `MissingSigningKey` if
    /// unset, empty, or not a valid 64-hex-char string.
    ///
    /// The env value is wrapped in `Zeroizing` so the heap
    /// allocation backing the read is wiped as soon as this
    /// function returns.
    pub fn from_env() -> Result<Self, TokenError> {
        let raw: Zeroizing<String> = Zeroizing::new(
            std::env::var(SIGNING_KEY_ENV).map_err(|_| TokenError::MissingSigningKey)?,
        );
        Self::from_hex_seed(&raw)
    }

    /// Hex-prefix fingerprint of the verifying key. Stable
    /// across boots for the same seed.
    pub fn fingerprint(&self) -> &str {
        &self.inner.fingerprint
    }

    /// Public verifying key derived from the seed. Operators
    /// distribute this to responders that need to verify
    /// tokens issued by this signer.
    pub fn verifying_key(&self) -> VerifyingKey {
        self.inner.verifying
    }

    fn sign(&self, payload: &[u8]) -> Signature {
        self.inner.signing.sign(payload)
    }
}

/// Verifier-side handle: a registry of fingerprint →
/// verifying-key entries. The admission gate looks up a wire
/// token's `signing_key_fingerprint` here to find the public
/// key it was signed under.
#[derive(Clone, Default)]
pub struct ApprovalKeySet {
    keys: Arc<HashMap<String, VerifyingKey>>,
}

impl ApprovalKeySet {
    /// Empty registry. Every verification attempt against an
    /// empty registry fails with `UnknownSigningKey`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a single-key registry from a signer. This is the
    /// common single-controller deployment shape.
    pub fn from_signer(signer: &ApprovalSigner) -> Self {
        let mut keys = HashMap::with_capacity(1);
        keys.insert(signer.fingerprint().to_string(), signer.verifying_key());
        Self {
            keys: Arc::new(keys),
        }
    }

    /// Build from an explicit set of `(fingerprint, key)`
    /// entries — used by tests + multi-key deployments where
    /// the responder accepts tokens from more than one issuer.
    pub fn from_entries(entries: impl IntoIterator<Item = (String, VerifyingKey)>) -> Self {
        let map: HashMap<String, VerifyingKey> = entries.into_iter().collect();
        Self {
            keys: Arc::new(map),
        }
    }

    /// True iff the registry has no entries.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Number of registered verifying keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Look up a verifying key by its fingerprint. Returns
    /// `None` when the registry has no entry for the
    /// fingerprint — the verify path maps this to
    /// `TokenError::UnknownSigningKey`.
    pub fn get(&self, fingerprint: &str) -> Option<&VerifyingKey> {
        self.keys.get(fingerprint)
    }
}

/// One signed approval token. Round-trips through the wire
/// format via [`Self::to_wire`] / [`Self::parse`]; the
/// signature field is always re-derived at issue time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalToken {
    /// Wire-format version byte. Always `TOKEN_VERSION` on
    /// freshly-minted tokens; tokens carrying any other value
    /// (including the legacy `0x01` HMAC version) are rejected
    /// at parse time with [`TokenError::TokenFormatDeprecated`].
    #[serde(default = "default_token_version_legacy")]
    pub version: u8,
    pub approval_id: String,
    pub method: String,
    pub subject_id: String,
    pub session_id: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub nonce: String,
    /// Hex-prefix BLAKE3 fingerprint of the verifying key
    /// (`compute_fingerprint(&verifying_key)`). Lets a verifier
    /// with multiple trusted keys pick the right one. Required
    /// from `TOKEN_VERSION` onwards.
    #[serde(default)]
    pub signing_key_fingerprint: String,
    /// Base64url-no-pad Ed25519 signature.
    pub signature: String,
}

fn default_token_version_legacy() -> u8 {
    // Tokens that omit the `version` field entirely are pre-
    // Ed25519 HMAC tokens — surface the legacy version byte so
    // the parse-time check rejects them with the explicit
    // `TokenFormatDeprecated` cause.
    TOKEN_VERSION_LEGACY_HMAC
}

impl ApprovalToken {
    /// Mint + sign a new token. Returns the wire-encoded
    /// (base64url-no-pad of the JSON) form.
    ///
    /// `ttl_ms` is the lifetime from `issued_at_ms`. Tokens
    /// MUST have non-zero TTL — a token that expires the
    /// moment it is minted is operationally useless. A
    /// `ttl_ms <= 0` returns `Expired` immediately so the call
    /// site catches the bug at issue time, not at verify time.
    pub fn issue(
        approval_id: &str,
        method: &str,
        subject_id: &str,
        session_id: &str,
        issued_at_ms: i64,
        ttl_ms: i64,
        signer: &ApprovalSigner,
    ) -> Result<String, TokenError> {
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
        let fingerprint = signer.fingerprint().to_string();
        let canonical = canonical_signing_bytes(
            TOKEN_VERSION,
            approval_id,
            method,
            subject_id,
            session_id,
            issued_at_ms,
            expires_at_ms,
            &nonce,
            &fingerprint,
        );
        let sig = signer.sign(canonical.as_bytes());
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes());
        let tok = Self {
            version: TOKEN_VERSION,
            approval_id: approval_id.into(),
            method: method.into(),
            subject_id: subject_id.into(),
            session_id: session_id.into(),
            issued_at_ms,
            expires_at_ms,
            nonce,
            signing_key_fingerprint: fingerprint,
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
    /// Rejects legacy HMAC tokens (version `0x01` or missing
    /// version field) with [`TokenError::TokenFormatDeprecated`].
    /// Does NOT verify the signature; callers MUST follow up
    /// with [`Self::verify_signature`].
    pub fn parse(wire: &str) -> Result<Self, TokenError> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(wire)
            .map_err(|e| TokenError::MalformedEncoding(e.to_string()))?;
        let tok: Self = serde_json::from_slice(&raw)
            .map_err(|e| TokenError::MalformedPayload(e.to_string()))?;
        if tok.version != TOKEN_VERSION {
            return Err(TokenError::TokenFormatDeprecated {
                got: tok.version,
                expected: TOKEN_VERSION,
            });
        }
        Ok(tok)
    }

    /// Verify the Ed25519 signature against the registered
    /// verifying key identified by
    /// [`Self::signing_key_fingerprint`]. Rejects unknown
    /// fingerprints with [`TokenError::UnknownSigningKey`] and
    /// signature-mismatch with [`TokenError::BadSignature`].
    pub fn verify_signature(&self, keyset: &ApprovalKeySet) -> Result<(), TokenError> {
        if keyset.is_empty() {
            return Err(TokenError::MissingSigningKey);
        }
        let verifying = keyset
            .get(&self.signing_key_fingerprint)
            .ok_or_else(|| TokenError::UnknownSigningKey(self.signing_key_fingerprint.clone()))?;
        let sig_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(self.signature.as_bytes())
            .map_err(|_| TokenError::BadSignature)?;
        if sig_bytes.len() != 64 {
            return Err(TokenError::BadSignature);
        }
        let mut sig_arr = [0u8; 64];
        sig_arr.copy_from_slice(&sig_bytes);
        let signature = Signature::from_bytes(&sig_arr);
        let canonical = canonical_signing_bytes(
            self.version,
            &self.approval_id,
            &self.method,
            &self.subject_id,
            &self.session_id,
            self.issued_at_ms,
            self.expires_at_ms,
            &self.nonce,
            &self.signing_key_fingerprint,
        );
        verifying
            .verify_strict(canonical.as_bytes(), &signature)
            .map_err(|_| TokenError::BadSignature)
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
    /// the verified caller. Byte-for-byte compare.
    pub fn check_subject(&self, caller_subject: &str) -> Result<(), TokenError> {
        use subtle::ConstantTimeEq;
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
    /// `approval_id` match.
    pub fn blocklist_key(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(self.nonce.as_bytes());
        h.update(b"|");
        h.update(self.approval_id.as_bytes());
        h.finalize().to_hex().to_string()
    }
}

/// Compute the verification-key fingerprint. BLAKE3 of the
/// 32-byte verifying-key bytes, truncated to the first
/// `FINGERPRINT_HEX_LEN` hex characters.
pub fn compute_fingerprint(key: &VerifyingKey) -> String {
    let digest = blake3::hash(&key.to_bytes());
    let full = digest.to_hex();
    full[..FINGERPRINT_HEX_LEN].to_string()
}

#[allow(clippy::too_many_arguments)]
fn canonical_signing_bytes(
    version: u8,
    approval_id: &str,
    method: &str,
    subject_id: &str,
    session_id: &str,
    issued_at_ms: i64,
    expires_at_ms: i64,
    nonce: &str,
    fingerprint: &str,
) -> String {
    // Prepend the version byte as an ASCII hex pair so the
    // canonical pre-image is itself UTF-8 — Ed25519 doesn't
    // care, but a string keeps the test-side equality checks
    // and the doc match.
    format!(
        "{version:02x}|{approval_id}|{method}|{subject_id}|{session_id}|\
         {issued_at_ms}|{expires_at_ms}|{nonce}|{fingerprint}"
    )
}

fn mint_nonce() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Read the Ed25519 signing-key seed from the environment and
/// return a fully-built [`ApprovalSigner`]. Kept as a free
/// function so the controller startup can call it before any
/// `DispatchBridge` exists.
pub fn signer_from_env() -> Result<ApprovalSigner, TokenError> {
    ApprovalSigner::from_env()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_signer() -> ApprovalSigner {
        ApprovalSigner::from_seed([7u8; 32])
    }

    fn other_signer() -> ApprovalSigner {
        ApprovalSigner::from_seed([42u8; 32])
    }

    #[test]
    fn issue_then_parse_round_trips_every_field() {
        let signer = fixture_signer();
        let wire = ApprovalToken::issue(
            "approval-1",
            "tool.web_read",
            "subject-abc",
            "session-7",
            1_700_000_000_000,
            60_000,
            &signer,
        )
        .unwrap();
        let parsed = ApprovalToken::parse(&wire).unwrap();
        assert_eq!(parsed.version, TOKEN_VERSION);
        assert_eq!(parsed.approval_id, "approval-1");
        assert_eq!(parsed.method, "tool.web_read");
        assert_eq!(parsed.subject_id, "subject-abc");
        assert_eq!(parsed.session_id, "session-7");
        assert_eq!(parsed.issued_at_ms, 1_700_000_000_000);
        assert_eq!(parsed.expires_at_ms, 1_700_000_060_000);
        assert_eq!(parsed.nonce.len(), 64);
        assert_eq!(parsed.signing_key_fingerprint, signer.fingerprint());
        // Ed25519 signature is 64 bytes → base64url-no-pad is
        // ceil(64*4/3) = ~86 chars.
        assert!(parsed.signature.len() >= 80);
    }

    #[test]
    fn ed25519_token_is_accepted_when_signature_is_valid() {
        // P1 test: "An Ed25519 token is accepted when the
        // signature is valid".
        let signer = fixture_signer();
        let keyset = ApprovalKeySet::from_signer(&signer);
        let wire = ApprovalToken::issue("a", "m", "s", "sess", 1_000, 60_000, &signer).unwrap();
        let parsed = ApprovalToken::parse(&wire).unwrap();
        parsed.verify_signature(&keyset).expect("verify accepts");
    }

    #[test]
    fn hmac_token_from_old_format_is_rejected_with_format_deprecated() {
        // P1 test: "An HMAC token from the old format is
        // rejected with TOKEN_FORMAT_DEPRECATED".
        //
        // Hand-build a legacy-shape token: no `version` field
        // and a 64-hex-char signature (the old HMAC tag was 64
        // chars). serde defaults `version` to 0x01 on parse so
        // the deprecation guard fires.
        let legacy_json = serde_json::json!({
            "approval_id": "a",
            "method": "m",
            "subject_id": "s",
            "session_id": "sess",
            "issued_at_ms": 1_000_i64,
            "expires_at_ms": 61_000_i64,
            "nonce": "00".repeat(32),
            "signature": "0".repeat(64),
        });
        let wire = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&legacy_json).unwrap());
        let err = ApprovalToken::parse(&wire).unwrap_err();
        match err {
            TokenError::TokenFormatDeprecated { got, expected } => {
                assert_eq!(got, TOKEN_VERSION_LEGACY_HMAC);
                assert_eq!(expected, TOKEN_VERSION);
            }
            other => panic!("expected TokenFormatDeprecated, got {other:?}"),
        }
        // Explicit `version: 0x01` is also rejected.
        let mut as_obj = legacy_json.clone();
        as_obj["version"] = serde_json::json!(0x01_u8);
        let wire2 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&as_obj).unwrap());
        match ApprovalToken::parse(&wire2) {
            Err(TokenError::TokenFormatDeprecated { got, .. }) => assert_eq!(got, 0x01),
            other => panic!("expected TokenFormatDeprecated, got {other:?}"),
        }
    }

    #[test]
    fn modifying_any_field_invalidates_signature() {
        // P1 test: "Modifying any field in the token
        // invalidates the signature".
        let signer = fixture_signer();
        let keyset = ApprovalKeySet::from_signer(&signer);
        let wire = ApprovalToken::issue(
            "a-orig",
            "tool.web_read",
            "subject-1",
            "sess-1",
            1_000,
            60_000,
            &signer,
        )
        .unwrap();
        let original = ApprovalToken::parse(&wire).unwrap();
        // Method tampering.
        {
            let mut t = original.clone();
            t.method = "tool.terminal".into();
            assert_eq!(t.verify_signature(&keyset), Err(TokenError::BadSignature));
        }
        // Subject_id tampering.
        {
            let mut t = original.clone();
            t.subject_id = "subject-evil".into();
            assert_eq!(t.verify_signature(&keyset), Err(TokenError::BadSignature));
        }
        // Approval_id tampering.
        {
            let mut t = original.clone();
            t.approval_id = "a-forged".into();
            assert_eq!(t.verify_signature(&keyset), Err(TokenError::BadSignature));
        }
        // Expiry extension.
        {
            let mut t = original.clone();
            t.expires_at_ms += 60_000;
            assert_eq!(t.verify_signature(&keyset), Err(TokenError::BadSignature));
        }
        // Nonce tampering.
        {
            let mut t = original.clone();
            t.nonce = "ff".repeat(32);
            assert_eq!(t.verify_signature(&keyset), Err(TokenError::BadSignature));
        }
        // Session tampering.
        {
            let mut t = original.clone();
            t.session_id = "sess-other".into();
            assert_eq!(t.verify_signature(&keyset), Err(TokenError::BadSignature));
        }
        // Signature byte-flip.
        {
            let mut t = original.clone();
            let mut bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(t.signature.as_bytes())
                .unwrap();
            bytes[0] ^= 0x01;
            t.signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
            assert_eq!(t.verify_signature(&keyset), Err(TokenError::BadSignature));
        }
        // Original verifies untouched.
        original.verify_signature(&keyset).unwrap();
    }

    #[test]
    fn token_signed_with_different_key_is_rejected() {
        // P1 test: "A token signed with a different key is
        // rejected".
        let signer_a = fixture_signer();
        let signer_b = other_signer();
        // Bridge knows only signer_a's verifying key.
        let keyset = ApprovalKeySet::from_signer(&signer_a);
        let wire = ApprovalToken::issue("a", "m", "s", "sess", 1_000, 60_000, &signer_b).unwrap();
        let parsed = ApprovalToken::parse(&wire).unwrap();
        // The fingerprint on the wire is signer_b's — the
        // keyset has no entry for it.
        match parsed.verify_signature(&keyset) {
            Err(TokenError::UnknownSigningKey(fp)) => {
                assert_eq!(fp, signer_b.fingerprint());
            }
            other => panic!("expected UnknownSigningKey, got {other:?}"),
        }
        // If the bridge ALSO trusts signer_b BUT a wire
        // signature came from signer_a (an attacker who knew
        // the fingerprint but not the seed), verify rejects
        // with BadSignature.
        let mixed_keyset = ApprovalKeySet::from_entries([
            (signer_a.fingerprint().to_string(), signer_a.verifying_key()),
            (signer_b.fingerprint().to_string(), signer_b.verifying_key()),
        ]);
        // Mint with signer_a but forge fingerprint to signer_b.
        let mut forged = ApprovalToken::parse(
            &ApprovalToken::issue("a", "m", "s", "sess", 1_000, 60_000, &signer_a).unwrap(),
        )
        .unwrap();
        forged.signing_key_fingerprint = signer_b.fingerprint().to_string();
        // The fingerprint is in the keyset → the lookup
        // resolves to signer_b's verifying key, which then
        // fails to verify signer_a's signature.
        assert_eq!(
            forged.verify_signature(&mixed_keyset),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn method_mismatch_is_caught_independent_of_signature() {
        let signer = fixture_signer();
        let keyset = ApprovalKeySet::from_signer(&signer);
        let wire = ApprovalToken::issue("a", "tool.web_read", "s", "sess", 1_000, 60_000, &signer)
            .unwrap();
        let parsed = ApprovalToken::parse(&wire).unwrap();
        parsed.verify_signature(&keyset).unwrap();
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
        let signer = fixture_signer();
        let wire = ApprovalToken::issue("a", "m", "subject-alice", "sess", 1_000, 60_000, &signer)
            .unwrap();
        let parsed = ApprovalToken::parse(&wire).unwrap();
        assert_eq!(
            parsed.check_subject("subject-bob"),
            Err(TokenError::SubjectMismatch)
        );
        assert_eq!(
            parsed.check_subject("subject-evil!"),
            Err(TokenError::SubjectMismatch)
        );
        parsed.check_subject("subject-alice").expect("match");
    }

    #[test]
    fn expired_token_is_rejected() {
        let signer = fixture_signer();
        let wire = ApprovalToken::issue("a", "m", "s", "sess", 1_000, 60_000, &signer).unwrap();
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

    #[test]
    fn ttl_boundary_admits_one_ms_before_expiry() {
        let signer = fixture_signer();
        let wire = ApprovalToken::issue("a", "m", "s", "sess", 1_000, 60_000, &signer).unwrap();
        let tok = ApprovalToken::parse(&wire).unwrap();
        assert_eq!(tok.expires_at_ms, 61_000);
        tok.check_not_expired(60_999)
            .expect("now = expires - 1 must admit");
    }

    #[test]
    fn ttl_boundary_rejects_exactly_at_expiry() {
        let signer = fixture_signer();
        let wire = ApprovalToken::issue("a", "m", "s", "sess", 1_000, 60_000, &signer).unwrap();
        let tok = ApprovalToken::parse(&wire).unwrap();
        match tok.check_not_expired(61_000) {
            Err(TokenError::Expired {
                now_ms,
                expires_at_ms,
            }) => {
                assert_eq!(now_ms, 61_000);
                assert_eq!(expires_at_ms, 61_000);
            }
            other => panic!("expected Expired at the exact boundary, got {other:?}"),
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
        let signer = fixture_signer();
        let err = ApprovalToken::issue("a|injected", "m", "s", "sess", 1_000, 60_000, &signer)
            .unwrap_err();
        assert_eq!(err, TokenError::ForbiddenDelimiter("approval_id"));
    }

    #[test]
    fn issue_rejects_non_positive_ttl() {
        let signer = fixture_signer();
        match ApprovalToken::issue("a", "m", "s", "sess", 0, 0, &signer) {
            Err(TokenError::Expired { .. }) => {}
            other => panic!("expected Expired, got {other:?}"),
        }
    }

    #[test]
    fn matched_rule_is_distinct_per_failure_mode() {
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
            TokenError::UnknownSigningKey("xxx".into()).matched_rule(),
            TokenError::Store(String::new()).matched_rule(),
            TokenError::TokenFormatDeprecated {
                got: 0x01,
                expected: TOKEN_VERSION,
            }
            .matched_rule(),
        ];
        for r in &v {
            assert!(r.starts_with("approval_token_"));
        }
    }

    #[test]
    fn blocklist_key_is_stable_per_nonce_and_approval_id() {
        let signer = fixture_signer();
        let wire = ApprovalToken::issue("a1", "m", "s", "sess", 1_000, 60_000, &signer).unwrap();
        let parsed = ApprovalToken::parse(&wire).unwrap();
        let k1 = parsed.blocklist_key();
        let k2 = parsed.blocklist_key();
        assert_eq!(k1, k2);
        let mut p2 = parsed.clone();
        p2.approval_id = "a2".into();
        assert_ne!(k1, p2.blocklist_key());
    }

    #[test]
    fn issue_with_distinct_nonces_produces_distinct_blocklist_keys() {
        let signer = fixture_signer();
        let w1 = ApprovalToken::issue("a", "m", "s", "sess", 1, 60_000, &signer).unwrap();
        let w2 = ApprovalToken::issue("a", "m", "s", "sess", 2, 60_000, &signer).unwrap();
        let t1 = ApprovalToken::parse(&w1).unwrap();
        let t2 = ApprovalToken::parse(&w2).unwrap();
        assert_ne!(t1.nonce, t2.nonce);
        assert_ne!(t1.blocklist_key(), t2.blocklist_key());
    }

    #[test]
    fn fingerprint_is_stable_across_signer_constructions() {
        let s1 = ApprovalSigner::from_seed([3u8; 32]);
        let s2 = ApprovalSigner::from_seed([3u8; 32]);
        assert_eq!(s1.fingerprint(), s2.fingerprint());
        assert_eq!(s1.fingerprint().len(), FINGERPRINT_HEX_LEN);
        let other = ApprovalSigner::from_seed([4u8; 32]);
        assert_ne!(s1.fingerprint(), other.fingerprint());
    }

    #[test]
    fn from_hex_seed_rejects_malformed_input() {
        assert_eq!(
            ApprovalSigner::from_hex_seed("not-hex").unwrap_err(),
            TokenError::MissingSigningKey
        );
        assert_eq!(
            ApprovalSigner::from_hex_seed("ab").unwrap_err(),
            TokenError::MissingSigningKey
        );
        // Wrong char count.
        assert_eq!(
            ApprovalSigner::from_hex_seed(&"a".repeat(63)).unwrap_err(),
            TokenError::MissingSigningKey
        );
        // Right length, invalid hex.
        assert_eq!(
            ApprovalSigner::from_hex_seed(&"zz".repeat(32)).unwrap_err(),
            TokenError::MissingSigningKey
        );
        // Valid.
        let ok = ApprovalSigner::from_hex_seed(&"ab".repeat(32));
        assert!(ok.is_ok());
    }

    #[test]
    fn empty_keyset_rejects_verify() {
        let signer = fixture_signer();
        let wire = ApprovalToken::issue("a", "m", "s", "sess", 1, 60_000, &signer).unwrap();
        let parsed = ApprovalToken::parse(&wire).unwrap();
        let empty = ApprovalKeySet::new();
        assert_eq!(
            parsed.verify_signature(&empty),
            Err(TokenError::MissingSigningKey)
        );
    }
}
