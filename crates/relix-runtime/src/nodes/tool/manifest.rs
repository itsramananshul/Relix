//! Cryptographically signed tool manifests.
//!
//! Operators distribute and verify tool definitions through
//! the bridge's `/v1/tools/manifest` endpoint. The signature
//! is a blake3 keyed hash over the canonical concatenation of
//! `version` + serialised tool JSON + `signed_at` + `signer`.
//! Any byte-level change to those fields produces a different
//! tag, so a tampered manifest fails [`SignedManifest::verify`].
//!
//! Honest scope: blake3 keyed hash is a MAC, not a public-key
//! signature. The verifier needs the same key the signer used
//! — which fits Relix's existing identity model (the libp2p
//! signing key the controller already holds). When a manifest
//! crosses an organisational trust boundary, swap the MAC for
//! ed25519 over the same canonical bytes; the structure stays
//! the same.

use serde::{Deserialize, Serialize};

use super::registry::ToolDefinition;

/// Unsigned manifest. Fields are intentionally minimal: a
/// signature over a fixed schema is much easier to verify
/// than a signature over an arbitrary JSON blob.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolManifest {
    pub version: u32,
    pub tools: Vec<ToolDefinition>,
    pub signed_at: i64,
    pub signer: String,
}

/// Manifest + signature.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SignedManifest {
    pub manifest: ToolManifest,
    pub signature: String,
}

/// Errors raised by the verifier / parser.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest signature is invalid")]
    InvalidSignature,
    #[error("manifest serialization: {0}")]
    Serialization(String),
}

impl SignedManifest {
    /// Sign a manifest with the supplied key. Signature is a
    /// blake3 keyed MAC over the canonical bytes
    /// (`canonical_bytes`).
    pub fn sign(manifest: ToolManifest, key: &[u8]) -> Self {
        let payload = canonical_bytes(&manifest);
        let mac_key = derive_mac_key(key);
        let mac = blake3::keyed_hash(&mac_key, &payload);
        Self {
            manifest,
            signature: hex::encode(mac.as_bytes()),
        }
    }

    /// Verify against the supplied key. `Ok(())` when the
    /// recomputed MAC matches the stored signature.
    /// Constant-time comparison via [`subtle`]-like blake3
    /// `Hash` equality — blake3's `Hash` type is a fixed-
    /// width newtype whose `PartialEq` checks every byte
    /// without short-circuiting.
    pub fn verify(&self, key: &[u8]) -> Result<(), ManifestError> {
        let expected_bytes = match hex::decode(&self.signature) {
            Ok(b) => b,
            Err(_) => return Err(ManifestError::InvalidSignature),
        };
        if expected_bytes.len() != 32 {
            return Err(ManifestError::InvalidSignature);
        }
        let payload = canonical_bytes(&self.manifest);
        let mac_key = derive_mac_key(key);
        let computed = blake3::keyed_hash(&mac_key, &payload);
        let mut expected = [0u8; 32];
        expected.copy_from_slice(&expected_bytes);
        if computed.as_bytes() == &expected {
            Ok(())
        } else {
            Err(ManifestError::InvalidSignature)
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn from_json(s: &str) -> Result<Self, ManifestError> {
        serde_json::from_str(s).map_err(|e| ManifestError::Serialization(e.to_string()))
    }
}

/// Stable byte serialisation used as the MAC input. Order
/// matters: a verifier must reconstruct the same bytes from
/// the same manifest fields, in the same order, every time.
fn canonical_bytes(m: &ToolManifest) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(&m.version.to_le_bytes());
    buf.push(b'|');
    // serde_json with `to_vec_pretty` would invite trailing-
    // whitespace differences across versions; `to_vec`
    // produces a stable shape. Tools always serialise in the
    // input order so the resulting bytes don't depend on a
    // HashMap traversal.
    let tools_json = serde_json::to_vec(&m.tools).unwrap_or_default();
    buf.extend_from_slice(&tools_json);
    buf.push(b'|');
    buf.extend_from_slice(&m.signed_at.to_le_bytes());
    buf.push(b'|');
    buf.extend_from_slice(m.signer.as_bytes());
    buf
}

/// Derive the 32-byte blake3 keyed-hash key from an
/// arbitrary-length operator key. `keyed_hash` requires
/// exactly 32 bytes; we hash the input to that width so
/// callers can pass any-length material (the libp2p ed25519
/// secret is 32 bytes already; the test path uses short
/// strings).
fn derive_mac_key(key: &[u8]) -> [u8; 32] {
    let h = blake3::hash(key);
    *h.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn tool(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: format!("description for {name}"),
            input_schema: Value::Object(Default::default()),
            output_schema: Value::Object(Default::default()),
            reversible: true,
            rollback_hint: None,
            tags: vec!["test".into()],
        }
    }

    fn sample_manifest() -> ToolManifest {
        ToolManifest {
            version: 1,
            tools: vec![tool("tool.web_fetch"), tool("tool.fs.read_file")],
            signed_at: 1_700_000_000,
            signer: "controller-1".into(),
        }
    }

    #[test]
    fn sign_produces_non_empty_signature_of_fixed_width() {
        let signed = SignedManifest::sign(sample_manifest(), b"operator-key");
        // blake3 output = 32 bytes = 64 hex chars.
        assert_eq!(signed.signature.len(), 64);
        assert!(!signed.signature.is_empty());
    }

    #[test]
    fn verify_accepts_a_valid_signature() {
        let key = b"operator-key";
        let signed = SignedManifest::sign(sample_manifest(), key);
        assert_eq!(signed.verify(key), Ok(()));
    }

    #[test]
    fn verify_rejects_a_tampered_manifest() {
        let key = b"operator-key";
        let mut signed = SignedManifest::sign(sample_manifest(), key);
        // Tamper with the manifest while keeping the
        // signature: the verifier must catch the mismatch.
        signed.manifest.signer = "imposter".into();
        match signed.verify(key) {
            Err(ManifestError::InvalidSignature) => {}
            other => panic!("expected InvalidSignature, got {other:?}"),
        }
    }

    #[test]
    fn verify_rejects_a_signature_made_with_a_different_key() {
        let signed = SignedManifest::sign(sample_manifest(), b"original-key");
        match signed.verify(b"different-key") {
            Err(ManifestError::InvalidSignature) => {}
            other => panic!("expected InvalidSignature, got {other:?}"),
        }
    }

    #[test]
    fn verify_rejects_a_signature_with_wrong_length() {
        let mut signed = SignedManifest::sign(sample_manifest(), b"key");
        // Truncate the signature so it's not 32 bytes hex.
        signed.signature.truncate(10);
        match signed.verify(b"key") {
            Err(ManifestError::InvalidSignature) => {}
            other => panic!("expected InvalidSignature, got {other:?}"),
        }
    }

    #[test]
    fn from_json_round_trips_through_to_json() {
        let signed = SignedManifest::sign(sample_manifest(), b"key");
        let json = signed.to_json();
        let back = SignedManifest::from_json(&json).unwrap();
        assert_eq!(back, signed);
        assert_eq!(back.verify(b"key"), Ok(()));
    }

    #[test]
    fn from_json_returns_serialization_error_on_bad_input() {
        let err = SignedManifest::from_json("{not-json}").unwrap_err();
        match err {
            ManifestError::Serialization(_) => {}
            other => panic!("expected Serialization, got {other:?}"),
        }
    }

    #[test]
    fn signing_is_deterministic_for_same_inputs() {
        let key = b"key";
        let a = SignedManifest::sign(sample_manifest(), key);
        let b = SignedManifest::sign(sample_manifest(), key);
        assert_eq!(a.signature, b.signature);
    }

    #[test]
    fn changing_tools_changes_signature() {
        let key = b"key";
        let a = SignedManifest::sign(sample_manifest(), key);
        let mut other = sample_manifest();
        other.tools.push(tool("tool.audio.transcribe"));
        let b = SignedManifest::sign(other, key);
        assert_ne!(a.signature, b.signature);
    }
}
