//! PART 2 — centralised mesh-call builder.
//!
//! Every bridge handler that issues a mesh call MUST go
//! through this module's [`build_mesh_request`] helper so the
//! per-request tenant id (derived from auth, per PART 5)
//! flows into the outbound `RequestEnvelope` automatically.
//! Handlers must NOT construct a `RequestEnvelope` (or call
//! `relix_runtime::dispatch::build_request` /
//! `build_request_with_surface`) directly — those entry
//! points leave `tenant_id = None` and bypass the isolation
//! contract.
//!
//! Part 3 of the rollout migrates every existing handler /
//! helper to call this function. Part 2 (this commit) ships
//! the helper + the canonical extractor so the migration
//! has a single landing point.

use std::sync::Arc;

use axum::http::HeaderMap;
use relix_core::bundle::Bundle;
use relix_runtime::dispatch::build_request_with_tenant;
use relix_runtime::manifest::MeshClient;

use crate::config::AppState;
use crate::tenant::{TenantConfig, TenantResolution, extract_tenant_id};

/// Returned by [`build_mesh_request`] when the resolver
/// short-circuits with a 401-equivalent outcome. Distinct
/// from the normal envelope bytes so handlers can map it
/// onto an HTTP 401 response.
#[allow(dead_code)] // PART 3 handlers will exercise these variants.
#[derive(Debug, thiserror::Error)]
pub enum BuildMeshRequestError {
    /// PART 5 multi-tenant mode reject — no binding /
    /// no credential. Handlers MUST surface this as HTTP
    /// 401 with the documented copy.
    #[error(
        "no tenant binding found for this credential; \
         configure a tenant binding in [auth.tenant_bindings]"
    )]
    MissingBinding,
    /// Mesh client not initialised. Treated as HTTP 503
    /// (the legacy `call_peer_*` helpers already surface
    /// this shape).
    #[error("bridge mesh client not initialised")]
    MeshUnavailable,
}

/// Centralised builder. Resolves the per-request tenant via
/// [`extract_tenant_id`], reads the bridge's identity bundle
/// and transport deadline from `state`, and produces the
/// encoded `RequestEnvelope` bytes ready for
/// `mesh.call(alias, envelope)`.
///
/// Returns:
/// - `Ok((mesh_arc, envelope_bytes))` on success — caller
///   issues the mesh call and decodes the response.
/// - `Err(MissingBinding)` when multi_tenant_mode = true AND
///   the request has no valid credential ↔ binding.
/// - `Err(MeshUnavailable)` when `state.mesh_client` is None.
///
/// All other extraction (source IP, bearer token, header)
/// happens inside the function so the handler signature stays
/// thin: `(state, headers, method, args)`.
#[allow(dead_code)] // PART 3 handlers will replace direct `build_request` calls with this.
pub fn build_mesh_request(
    state: &AppState,
    headers: &HeaderMap,
    source_ip: std::net::IpAddr,
    method: &str,
    args: impl Into<Vec<u8>>,
) -> Result<(Arc<MeshClient>, Vec<u8>), BuildMeshRequestError> {
    let mesh = state
        .mesh_client
        .as_ref()
        .ok_or(BuildMeshRequestError::MeshUnavailable)?
        .clone();
    let cfg = TenantConfig::from_auth_section(&state.cfg.auth);
    let tenant_id = match extract_tenant_id(
        &cfg.tenant_bindings,
        &cfg.trusted_origins,
        cfg.multi_tenant_mode,
        source_ip,
        headers,
    ) {
        TenantResolution::Resolved(t) => Some(t),
        TenantResolution::SingleTenant => None,
        TenantResolution::MissingBinding => {
            return Err(BuildMeshRequestError::MissingBinding);
        }
    };
    let bytes = build_envelope_with_tenant(
        method,
        args.into(),
        state.identity_bundle.clone(),
        state.cfg.transport.deadline_secs.clamp(5, 120),
        tenant_id,
    );
    Ok((mesh, bytes))
}

/// Thin shim around
/// [`relix_runtime::dispatch::build_request_with_tenant`] —
/// exists so a test can construct the envelope bytes without
/// going through the full `build_mesh_request` (which needs a
/// full `AppState`).
#[allow(dead_code)] // PART 3 production callers exercise this through `build_mesh_request`.
pub fn build_envelope_with_tenant(
    method: &str,
    args: Vec<u8>,
    identity: Bundle,
    deadline_secs: i64,
    tenant_id: Option<String>,
) -> Vec<u8> {
    build_request_with_tenant(
        method,
        args,
        identity,
        deadline_secs,
        None,
        None,
        None,
        tenant_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_runtime::dispatch::decode_response;
    use relix_runtime::transport::envelope::RequestEnvelope;

    #[test]
    fn fix_part2_build_envelope_with_tenant_stamps_field_on_wire() {
        let bundle = mock_bundle();
        let bytes = build_envelope_with_tenant(
            "memory.search",
            b"{}".to_vec(),
            bundle.clone(),
            30,
            Some("acme".into()),
        );
        let req: RequestEnvelope = relix_core::codec::decode(&bytes).expect("envelope decodes");
        assert_eq!(req.tenant_id.as_deref(), Some("acme"));
        assert_eq!(req.method, "memory.search");
    }

    #[test]
    fn fix_part2_build_envelope_with_none_tenant_leaves_field_unset() {
        let bundle = mock_bundle();
        let bytes = build_envelope_with_tenant("ai.chat", b"{}".to_vec(), bundle, 30, None);
        let req: RequestEnvelope = relix_core::codec::decode(&bytes).expect("envelope decodes");
        assert!(req.tenant_id.is_none());
    }

    /// Compile-test: `decode_response` is in the same module
    /// the helper imports — guard against a future refactor
    /// that moves the symbol elsewhere.
    #[test]
    fn fix_part2_decode_response_is_imported_from_the_right_module() {
        // `decode_response` is in scope; using it as a fn ptr
        // is the cheapest compile-test.
        let _f: fn(&[u8]) -> Result<_, _> = decode_response;
    }

    fn mock_bundle() -> Bundle {
        // Build a minimal but valid bundle via the identity
        // helper so the envelope decode path doesn't trip on
        // a malformed bundle.
        use ed25519_dalek::SigningKey;
        use rand::rngs::OsRng;
        use relix_core::identity::{IdentityBundle, issue_identity};
        use relix_core::types::NodeId;
        let root = SigningKey::generate(&mut OsRng);
        let subject = SigningKey::generate(&mut OsRng);
        let bundle = IdentityBundle {
            subject_id: NodeId::from_pubkey(&subject.verifying_key().to_bytes()),
            name: "tenant-test".into(),
            org_id: NodeId::from_pubkey(&root.verifying_key().to_bytes()),
            groups: vec!["chat".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        issue_identity(bundle, &root, 3600).expect("identity issues")
    }
}
