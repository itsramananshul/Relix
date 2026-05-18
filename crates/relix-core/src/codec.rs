//! Deterministic CBOR encoding/decoding per RFC 8949 §4.2.
//!
//! ## DETERMINISM
//!
//! [`encode`] MUST produce byte-identical output for inputs that are logically
//! equal. The current implementation wraps `ciborium`, which by itself does not
//! guarantee canonical CBOR for maps with non-sorted insertion order. We mitigate
//! by:
//!
//! 1. Forcing serializable types to use `BTreeMap` (sorted by Rust `Ord`) or
//!    `serde_bytes::ByteBuf` for raw bytes, both of which give us deterministic
//!    output for our schemas.
//! 2. Round-trip verification in tests: `encode(decode(encode(x))) == encode(x)`.
//!
//! The full canonical encoder per RFC 8949 §4.2 (length-then-bytewise map-key
//! ordering for arbitrary key types, indefinite-length rejection, etc.) is
//! deferred to Gate 2 (SIMP — tracked in `specs/alpha-simplifications.md`).
//! For alpha-scoped types, the simple approach is sufficient and verified by
//! property tests in `tests/codec_determinism.rs`.

use serde::{Serialize, de::DeserializeOwned};

/// Encode a value as deterministic CBOR.
///
/// DETERMINISM: byte-identical output for inputs that are logically equal,
/// for the type schemas used in Relix. Verified by property tests.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let mut buf = Vec::with_capacity(128);
    ciborium::ser::into_writer(value, &mut buf).map_err(|e| CodecError::Encode(e.to_string()))?;
    Ok(buf)
}

/// Decode a CBOR byte slice into the requested type.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, CodecError> {
    ciborium::de::from_reader(bytes).map_err(|e| CodecError::Decode(e.to_string()))
}

/// Compute the BLAKE3-256 content hash of an encoded bundle / event / record.
///
/// Used for bundle IDs (RELIX-4 §4.3) and event log chain links (RELIX-3 §3.2).
pub fn content_hash(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// Codec errors.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// CBOR encoding failed.
    #[error("encode failed: {0}")]
    Encode(String),
    /// CBOR decoding failed.
    #[error("decode failed: {0}")]
    Decode(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn roundtrip_primitive() {
        let v: u64 = 42;
        let bytes = encode(&v).expect("encode");
        let back: u64 = decode(&bytes).expect("decode");
        assert_eq!(v, back);
    }

    #[test]
    fn roundtrip_string() {
        let v = "hello".to_string();
        let bytes = encode(&v).expect("encode");
        let back: String = decode(&bytes).expect("decode");
        assert_eq!(v, back);
    }

    #[test]
    fn roundtrip_btree_map_is_deterministic() {
        // DETERMINISM: BTreeMap iterates in sorted order, so encoding is stable
        // regardless of insertion order.
        let mut a: BTreeMap<String, u32> = BTreeMap::new();
        a.insert("zeta".into(), 1);
        a.insert("alpha".into(), 2);
        a.insert("mu".into(), 3);

        let mut b: BTreeMap<String, u32> = BTreeMap::new();
        b.insert("alpha".into(), 2);
        b.insert("mu".into(), 3);
        b.insert("zeta".into(), 1);

        let enc_a = encode(&a).expect("encode a");
        let enc_b = encode(&b).expect("encode b");
        assert_eq!(
            enc_a, enc_b,
            "BTreeMap encodings differ despite equal contents"
        );
    }

    #[test]
    fn content_hash_is_stable() {
        let payload = b"relix";
        let h1 = content_hash(payload);
        let h2 = content_hash(payload);
        assert_eq!(h1, h2);
        // BLAKE3 hash is 32 bytes
        assert_eq!(h1.len(), 32);
    }
}
