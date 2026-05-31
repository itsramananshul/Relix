//! Identity bundles and verification.
//!
//! Alpha-grade implementation of `specs/identity-employees.md` §H.1. The
//! `IdentityBundle` here collapses AIC + GMC into a single bundle for simplicity
//! (SIMP-002); Gate 2 splits them per the spec.

use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::bundle::{Bundle, BundleError, BundleType, make_header};
use crate::codec::{self, CodecError};
use crate::types::NodeId;

/// IdentityBundle payload — the agent-employee record.
///
/// Carries everything the responder needs to evaluate policy: who the caller
/// is, what groups they belong to, what role, what clearance.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdentityBundle {
    /// Subject's node identity (matches the caller's PeerId at the transport layer at Gate 2).
    pub subject_id: NodeId,
    /// Human-readable name within the org.
    pub name: String,
    /// Issuing organization identifier (org root pubkey hash).
    pub org_id: NodeId,
    /// Group memberships. Policy matches against this set.
    pub groups: Vec<String>,
    /// Role within the org (`agent`, `human`, `admin`, `service`...).
    pub role: String,
    /// Clearance level (`public` / `internal` / `restricted` / `confidential`).
    pub clearance: String,
    /// Identities authorized to approve actions on behalf of this subject.
    /// Empty for the alpha (approval flows deferred).
    pub supervisors: Vec<String>,
}

/// Verified identity claims extracted after `validate_identity_bundle` succeeds.
///
/// Construction-private: only `validate_identity_bundle` constructs one.
/// Downstream code (policy, dispatch, audit) consumes this rather than the raw
/// bundle, so the type system prevents skipping verification.
#[derive(Clone, Debug)]
pub struct VerifiedIdentity {
    /// The subject's node id (also the calling peer's identity).
    pub subject_id: NodeId,
    /// Subject's human-readable name.
    pub name: String,
    /// Org id this identity belongs to.
    pub org_id: NodeId,
    /// Groups this caller is in.
    pub groups: Vec<String>,
    /// Role.
    pub role: String,
    /// Clearance level.
    pub clearance: String,
    /// Bundle id of the validated bundle. Used for audit cross-correlation.
    pub bundle_id: [u8; 32],
}

impl VerifiedIdentity {
    /// True iff the caller holds any of `required_groups`.
    pub fn has_any_group(&self, required_groups: &[String]) -> bool {
        required_groups.iter().any(|g| self.groups.contains(g))
    }
}

/// Issue an identity bundle. Used by `relix-cli identity mint`.
///
/// `signing_key` is the org-root key (alpha SIMP-002 single-key trust model).
pub fn issue_identity(
    payload: IdentityBundle,
    org_root_signing_key: &SigningKey,
    lifetime_secs: i64,
) -> Result<Bundle, IdentityError> {
    // The header `kid` is the org-root pubkey hash — the issuer, not the subject.
    let header = make_header(
        &org_root_signing_key.verifying_key(),
        BundleType::Identity,
        lifetime_secs,
    )?;
    let payload_bytes = codec::encode(&payload)?;
    let bundle = Bundle::sign(header, payload_bytes, org_root_signing_key)?;
    Ok(bundle)
}

/// Validate an identity bundle against a trusted org-root pubkey.
///
/// Steps mirror the admission pipeline (RELIX-1 §1.13 step 5):
/// 1. Validate the bundle envelope (sig, expiry, type).
/// 2. Decode the payload.
/// 3. Return [`VerifiedIdentity`] claims.
pub fn validate_identity_bundle(
    bundle: &Bundle,
    trusted_org_root: &VerifyingKey,
    now_unix_secs: i64,
) -> Result<VerifiedIdentity, IdentityError> {
    bundle.validate(trusted_org_root, BundleType::Identity, now_unix_secs)?;
    let payload: IdentityBundle = codec::decode(bundle.payload.as_ref())?;
    let bundle_id = bundle.bundle_id()?;

    // Cross-check: the bundle's kid is the org root; payload's org_id should match it.
    // This catches forged payloads claiming a different org under a legitimate root.
    if payload.org_id != bundle.header.kid {
        return Err(IdentityError::OrgMismatch);
    }

    Ok(VerifiedIdentity {
        subject_id: payload.subject_id,
        name: payload.name,
        org_id: payload.org_id,
        groups: payload.groups,
        role: payload.role,
        clearance: payload.clearance,
        bundle_id,
    })
}

/// Identity-layer errors.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// Codec failure.
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
    /// Bundle validation failed.
    #[error("bundle: {0}")]
    Bundle(#[from] BundleError),
    /// Payload's `org_id` does not match the bundle issuer.
    #[error("payload org_id does not match bundle issuer")]
    OrgMismatch,
}

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
    fn issue_validate_roundtrip() {
        let org_root = fresh_key();
        let subject_key = fresh_key();
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&subject_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into(), "tool-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id.clone(), &org_root, 3600).expect("issue");
        let verified =
            validate_identity_bundle(&bundle, &org_root.verifying_key(), now()).expect("validate");
        assert_eq!(verified.name, "alice");
        assert!(verified.has_any_group(&["chat-users".into()]));
        assert!(!verified.has_any_group(&["nonexistent".into()]));
    }

    #[test]
    fn forged_org_claim_rejected() {
        let real_root = fresh_key();
        let other_root = fresh_key();
        let subject_key = fresh_key();
        // Payload claims `other_root` is the org but bundle is signed by `real_root`.
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&subject_key.verifying_key().to_bytes()),
            name: "attacker".into(),
            org_id: NodeId::from_pubkey(&other_root.verifying_key().to_bytes()),
            groups: vec!["admin".into()],
            role: "agent".into(),
            clearance: "confidential".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &real_root, 3600).expect("issue");
        let err = validate_identity_bundle(&bundle, &real_root.verifying_key(), now())
            .expect_err("must reject");
        assert!(matches!(err, IdentityError::OrgMismatch));
    }

    #[test]
    fn wrong_trust_root_rejected() {
        let real_root = fresh_key();
        let attacker_root = fresh_key();
        let subject_key = fresh_key();
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&subject_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&real_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &real_root, 3600).expect("issue");
        // Try to validate against attacker's pubkey — must fail (kid mismatch).
        let err = validate_identity_bundle(&bundle, &attacker_root.verifying_key(), now())
            .expect_err("must reject");
        assert!(matches!(
            err,
            IdentityError::Bundle(BundleError::KidMismatch)
        ));
    }
}
