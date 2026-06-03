//! Signed CBOR bundle envelope — alpha simplification of RELIX-4 (full COSE_Sign1
//! lands at Gate 2 per SIMP — see `specs/alpha-simplifications.md`).
//!
//! ## Wire shape
//!
//! ```text
//! Bundle {
//!     header: BundleHeader {
//!         format_version: u8,         // currently 1
//!         alg: i8,                    // -8 = Ed25519
//!         kid: [u8; 32],              // issuer pubkey hash (= NodeId of issuer)
//!         bundle_type: BundleType,
//!         issued_at: i64,             // unix seconds
//!         not_before: i64,
//!         not_after: i64,
//!         bundle_serial: [u8; 16],    // freshness key
//!     },
//!     payload: bytes,                 // CBOR-encoded type-specific payload
//!     signature: [u8; 64],            // Ed25519(header || payload)
//! }
//! ```
//!
//! ## DETERMINISM
//!
//! Signature input is `encode(header)` concatenated with `payload` bytes verbatim.
//! Header encoding is deterministic per `codec` (BTreeMap-derived for the alpha
//! schema). Payload is supplied already-encoded by the caller. Identical inputs
//! produce identical signatures.

use crate::codec::{self, CodecError};
use crate::types::NodeId;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

/// Bundle types per RELIX-4 §4.4. Alpha subset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleType {
    /// Combined AIC + GMC for alpha (one bundle per identity, carries groups directly).
    /// Splits into separate `aic` + `gmc` bundles at Gate 2.
    Identity,
    /// Node manifest (RELIX-5).
    NodeManifest,
    /// Policy bundle (RELIX-1 step 9 / Cedar target at Gate 2).
    PolicyBundle,
}

/// Bundle header. Signed alongside payload.
///
/// DETERMINISM: encoded via [`codec::encode`] using a BTreeMap representation
/// indirectly through serde's derived ordering on this struct's field declaration
/// order. Field order here is the wire order; do not reorder.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BundleHeader {
    /// Bundle format version (RELIX-4). Currently 1.
    pub format_version: u8,
    /// Signature algorithm. -8 = Ed25519 per COSE_Algs.
    pub alg: i8,
    /// Issuer pubkey hash (BLAKE3-256 of Ed25519 pubkey bytes). Equals NodeId of issuer.
    pub kid: NodeId,
    /// Bundle type discriminator.
    pub bundle_type: BundleType,
    /// Unix-seconds issued-at.
    pub issued_at: i64,
    /// Unix-seconds not-before.
    pub not_before: i64,
    /// Unix-seconds not-after.
    pub not_after: i64,
    /// 16 random bytes — uniquely identifies this issuance, useful for revocation references.
    #[serde(with = "serde_bytes")]
    pub bundle_serial: [u8; 16],
}

/// A signed bundle on the wire.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bundle {
    /// Header (signature input).
    pub header: BundleHeader,
    /// Payload bytes (signature input).
    pub payload: ByteBuf,
    /// Ed25519 signature over `encode(header) || payload`.
    #[serde(with = "serde_bytes")]
    pub signature: [u8; 64],
}

impl Bundle {
    /// Sign a bundle from a header and pre-encoded payload.
    ///
    /// Caller is responsible for: (a) constructing the header with correct
    /// `kid` (matching the signing key), (b) supplying a deterministic CBOR
    /// encoding of the payload.
    pub fn sign(
        header: BundleHeader,
        payload: Vec<u8>,
        signing_key: &SigningKey,
    ) -> Result<Self, BundleError> {
        // Verify the header's kid matches the signing key — catches programmer error.
        let pubkey_bytes = signing_key.verifying_key().to_bytes();
        let expected_kid = NodeId::from_pubkey(&pubkey_bytes);
        if expected_kid != header.kid {
            return Err(BundleError::KidMismatch);
        }

        let header_bytes = codec::encode(&header)?;
        let mut sig_input = header_bytes;
        sig_input.extend_from_slice(&payload);
        let signature = signing_key.sign(&sig_input);

        Ok(Self {
            header,
            payload: ByteBuf::from(payload),
            signature: signature.to_bytes(),
        })
    }

    /// Bundle ID = BLAKE3-256 of the encoded bundle envelope.
    pub fn bundle_id(&self) -> Result<[u8; 32], BundleError> {
        let bytes = codec::encode(self)?;
        Ok(codec::content_hash(&bytes))
    }

    /// Verify signature against the supplied issuer public key.
    ///
    /// Does NOT validate trust chain, expiry, or revocation — see [`Bundle::validate`]
    /// for the full pipeline.
    pub fn verify_signature(&self, issuer_pubkey: &VerifyingKey) -> Result<(), BundleError> {
        // Confirm the supplied key matches the kid in the header.
        let supplied_kid = NodeId::from_pubkey(&issuer_pubkey.to_bytes());
        if supplied_kid != self.header.kid {
            return Err(BundleError::KidMismatch);
        }

        let header_bytes = codec::encode(&self.header)?;
        let mut sig_input = header_bytes;
        sig_input.extend_from_slice(self.payload.as_ref());

        let sig = ed25519_dalek::Signature::from_bytes(&self.signature);
        issuer_pubkey
            .verify(&sig_input, &sig)
            .map_err(|_| BundleError::SignatureInvalid)?;
        Ok(())
    }

    /// Full validation pipeline per RELIX-4 §4.7. Alpha-subset:
    /// 1. Verify supported `format_version`.
    /// 2. Verify supported `alg`.
    /// 3. Verify expected `bundle_type`.
    /// 4. Verify signature against the trusted-issuer key.
    /// 5. Verify `not_before ≤ now ≤ not_after` (±30s skew).
    ///
    /// SIMP-002: delegation chain length is 0 in the alpha. SIMP-003: revocation
    /// is by expiry only; no CRL check.
    pub fn validate(
        &self,
        issuer_pubkey: &VerifyingKey,
        expected_type: BundleType,
        now_unix_secs: i64,
    ) -> Result<(), BundleError> {
        if self.header.format_version != 1 {
            return Err(BundleError::FormatUnsupported(self.header.format_version));
        }
        if self.header.alg != -8 {
            return Err(BundleError::AlgUnsupported(self.header.alg));
        }
        if self.header.bundle_type != expected_type {
            return Err(BundleError::TypeMismatch {
                got: self.header.bundle_type,
                want: expected_type,
            });
        }
        const SKEW_SECS: i64 = 30;
        // SEC PART 6: checked arithmetic — a `now` at i64::MAX
        // would otherwise wrap and silently flip the comparison.
        let plus_skew = now_unix_secs
            .checked_add(SKEW_SECS)
            .ok_or(BundleError::ArithmeticOverflow)?;
        if plus_skew < self.header.not_before {
            return Err(BundleError::NotYetValid);
        }
        let minus_skew = now_unix_secs
            .checked_sub(SKEW_SECS)
            .ok_or(BundleError::ArithmeticOverflow)?;
        if minus_skew > self.header.not_after {
            return Err(BundleError::Expired);
        }
        self.verify_signature(issuer_pubkey)?;
        Ok(())
    }
}

/// Default lifetime for a locally-minted node/service identity bundle:
/// **365 days**. Self-hosted Relix meshes mint their own identities off a
/// local org root, so the lifetime is sized for unattended infra — long
/// enough that normal continuous operation never reaches expiry, while
/// keeping expiry as a real revocation backstop (SIMP-003). Short-lived
/// human-login bundles can still override this with a smaller value.
pub const DEFAULT_IDENTITY_LIFETIME_SECS: i64 = 365 * 24 * 60 * 60;

/// How long before `not_after` a bundle is considered due for renewal:
/// **30 days**. A running mesh (or a boot/`identity ensure` pass) that finds
/// a bundle inside this window re-mints it ahead of expiry so the identity
/// never lapses. Must be smaller than [`DEFAULT_IDENTITY_LIFETIME_SECS`].
pub const DEFAULT_RENEWAL_WINDOW_SECS: i64 = 30 * 24 * 60 * 60;

impl BundleHeader {
    /// Seconds remaining until `not_after` relative to `now_unix_secs`.
    /// Negative once the bundle has expired. Saturating so a pathological
    /// header can never panic or wrap.
    pub fn seconds_until_expiry(&self, now_unix_secs: i64) -> i64 {
        self.not_after.saturating_sub(now_unix_secs)
    }

    /// True when the bundle is at or past its renewal window — i.e. it has
    /// expired OR is within `renewal_window_secs` of `not_after`. This is the
    /// single decision both boot-time self-heal and the running-mesh renewal
    /// loop use to decide whether to re-mint ahead of expiry.
    pub fn needs_renewal(&self, now_unix_secs: i64, renewal_window_secs: i64) -> bool {
        self.seconds_until_expiry(now_unix_secs) <= renewal_window_secs
    }
}

/// Convenience: construct a header populated with `now()`-based timestamps and
/// a random `bundle_serial`. Returns `BundleError::ArithmeticOverflow` if the
/// `now ± skew` or `now + lifetime_secs` calculation would silently wrap in
/// release mode (only reachable with pathological `lifetime_secs` values or a
/// system clock at i64::MAX — both deserve a hard error).
pub fn make_header(
    issuer_pubkey: &VerifyingKey,
    bundle_type: BundleType,
    lifetime_secs: i64,
) -> Result<BundleHeader, BundleError> {
    use rand::RngCore;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    // SEC PART 6: checked arithmetic. The 30s skew + caller-
    // supplied lifetime is normally tiny, but a pathological
    // input (or a clock at i64::MAX) must surface as an error
    // instead of silently wrapping.
    let not_before = now.checked_sub(30).ok_or(BundleError::ArithmeticOverflow)?;
    let not_after = now
        .checked_add(lifetime_secs)
        .ok_or(BundleError::ArithmeticOverflow)?;
    let mut serial = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut serial);
    Ok(BundleHeader {
        format_version: 1,
        alg: -8,
        kid: NodeId::from_pubkey(&issuer_pubkey.to_bytes()),
        bundle_type,
        issued_at: now,
        not_before,
        not_after,
        bundle_serial: serial,
    })
}

/// Bundle-layer errors.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// Codec failure encoding/decoding the bundle.
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
    /// Supplied verifying key does not match the bundle's `kid`.
    #[error("issuer key id does not match bundle kid")]
    KidMismatch,
    /// Ed25519 signature verification failed.
    #[error("signature invalid")]
    SignatureInvalid,
    /// Unknown / unsupported `format_version`.
    #[error("unsupported format_version: {0}")]
    FormatUnsupported(u8),
    /// Unknown / unsupported signature algorithm.
    #[error("unsupported alg: {0}")]
    AlgUnsupported(i8),
    /// Bundle type mismatch.
    #[error("expected bundle type {want:?}, got {got:?}")]
    TypeMismatch {
        /// The bundle's actual type.
        got: BundleType,
        /// The type the caller expected.
        want: BundleType,
    },
    /// Bundle is not yet valid (now < not_before − skew).
    #[error("bundle not yet valid")]
    NotYetValid,
    /// Bundle expired (now > not_after + skew).
    #[error("bundle expired")]
    Expired,
    /// SEC PART 6: a `now ± lifetime` calculation in
    /// `make_header` (or any future arithmetic on the
    /// header timestamps) would silently wrap. Returned
    /// instead of permitting an i64 overflow.
    #[error("arithmetic overflow building bundle header")]
    ArithmeticOverflow,
}

/// Stable wire alias — useful when bundles are written/read as serde maps and the
/// caller wants typed access without re-deriving traits at call sites.
pub type BundleSig = [u8; 64];

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn fresh_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let key = fresh_key();
        let header = make_header(&key.verifying_key(), BundleType::Identity, 3600).unwrap();
        let payload = b"hello-payload".to_vec();
        let bundle = Bundle::sign(header, payload, &key).expect("sign");
        bundle
            .verify_signature(&key.verifying_key())
            .expect("verify");
    }

    #[test]
    fn tampered_payload_fails_verification() {
        let key = fresh_key();
        let header = make_header(&key.verifying_key(), BundleType::Identity, 3600).unwrap();
        let payload = b"original".to_vec();
        let mut bundle = Bundle::sign(header, payload, &key).expect("sign");
        bundle.payload = ByteBuf::from(b"tampered".to_vec());
        assert!(matches!(
            bundle.verify_signature(&key.verifying_key()),
            Err(BundleError::SignatureInvalid)
        ));
    }

    #[test]
    fn wrong_issuer_key_fails() {
        let key1 = fresh_key();
        let key2 = fresh_key();
        let header = make_header(&key1.verifying_key(), BundleType::Identity, 3600).unwrap();
        let bundle = Bundle::sign(header, b"x".to_vec(), &key1).expect("sign");
        // Validate with key2's pubkey — must fail at KidMismatch (caught before sig check).
        let err = bundle
            .verify_signature(&key2.verifying_key())
            .expect_err("must fail");
        assert!(matches!(err, BundleError::KidMismatch));
    }

    #[test]
    fn expired_bundle_rejected() {
        let key = fresh_key();
        let mut header = make_header(&key.verifying_key(), BundleType::Identity, 1).unwrap();
        // Set not_after to far in the past.
        header.not_after = now() - 10_000;
        let bundle = Bundle::sign(header, b"x".to_vec(), &key).expect("sign");
        let err = bundle
            .validate(&key.verifying_key(), BundleType::Identity, now())
            .expect_err("must fail");
        assert!(matches!(err, BundleError::Expired));
    }

    #[test]
    fn wrong_type_rejected() {
        let key = fresh_key();
        let header = make_header(&key.verifying_key(), BundleType::Identity, 3600).unwrap();
        let bundle = Bundle::sign(header, b"x".to_vec(), &key).expect("sign");
        let err = bundle
            .validate(&key.verifying_key(), BundleType::PolicyBundle, now())
            .expect_err("must fail");
        assert!(matches!(err, BundleError::TypeMismatch { .. }));
    }

    #[test]
    fn needs_renewal_fires_only_inside_window() {
        let key = fresh_key();
        // Fresh 1-year bundle: far from expiry → no renewal.
        let header = make_header(
            &key.verifying_key(),
            BundleType::Identity,
            DEFAULT_IDENTITY_LIFETIME_SECS,
        )
        .unwrap();
        let now = now();
        assert!(
            !header.needs_renewal(now, DEFAULT_RENEWAL_WINDOW_SECS),
            "a fresh 1-year bundle must not be due for renewal"
        );
        assert!(header.seconds_until_expiry(now) > DEFAULT_RENEWAL_WINDOW_SECS);

        // Simulate a near-expiry bundle: 10 days of life left, 30-day window.
        let mut near = header.clone();
        near.not_after = now + 10 * 24 * 60 * 60;
        assert!(
            near.needs_renewal(now, DEFAULT_RENEWAL_WINDOW_SECS),
            "a bundle 10 days from expiry must renew under a 30-day window"
        );

        // Already-expired bundle is also "needs renewal" (negative remaining).
        let mut expired = header;
        expired.not_after = now - 10_000;
        assert!(expired.needs_renewal(now, DEFAULT_RENEWAL_WINDOW_SECS));
        assert!(expired.seconds_until_expiry(now) < 0);
    }

    #[test]
    fn default_lifetime_is_one_year() {
        // Guards against an accidental regression back to a short lifetime.
        assert_eq!(DEFAULT_IDENTITY_LIFETIME_SECS, 365 * 24 * 60 * 60);
        // These are constants, so check the ordering at compile time; a
        // runtime assert on a constant draws clippy::assertions_on_constants.
        const _: () = assert!(DEFAULT_RENEWAL_WINDOW_SECS < DEFAULT_IDENTITY_LIFETIME_SECS);
    }

    #[test]
    fn bundle_id_is_stable() {
        let key = fresh_key();
        let header = make_header(&key.verifying_key(), BundleType::Identity, 3600).unwrap();
        let bundle = Bundle::sign(header, b"x".to_vec(), &key).expect("sign");
        let a = bundle.bundle_id().expect("id1");
        let b = bundle.bundle_id().expect("id2");
        assert_eq!(a, b);
    }
}
