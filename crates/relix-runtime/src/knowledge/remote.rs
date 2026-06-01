//! RELIX-7.16 GAP 3 — cross-node knowledge transfer.
//!
//! When a target agent's observations live on a different
//! memory node than the source agent, the local
//! `KnowledgeService` dispatches a
//! [`knowledge.accept_shared`] mesh capability call to the
//! remote node instead of writing locally. The remote node
//! verifies the inbound payload's ed25519 signature, runs
//! its own [`TrustChecker`], and inserts the observation
//! into its local layered store.
//!
//! Trust model:
//!
//! - The local node signs the canonical payload bytes with
//!   the node's signing key (loaded from `[identity]
//!   key_path` at boot).
//! - The receiver verifies the signature with the public key
//!   that the sender includes in the payload. Verification
//!   failure → [`crate::knowledge::trust::RejectReason::InvalidSignature`]
//!   and nothing lands on disk.
//! - Sender + receiver still run the full [`TrustChecker`]:
//!   group membership, layer guard, ownership, poison
//!   detection, quality floor, observation-count cap. The
//!   signature only proves the payload originated from a
//!   trusted node — it doesn't bypass the per-record
//!   policies.
//!
//! The dispatcher is a trait so unit tests can stub it
//! without spinning up libp2p. The production wiring builds
//! a `MeshKnowledgeDispatcher` over a `MeshClient` in
//! `controller_runtime`.

use std::collections::BTreeMap;
use std::sync::Arc;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::nodes::memory::schema::MemoryRecord;

use super::trust::RejectReason;

/// Canonical payload that travels between memory nodes.
///
/// The signature covers a deterministic byte sequence over
/// `source_node || source_agent || target_agent ||
/// record.id || record.text`. Operators cannot bypass the
/// signature by mutating the record AFTER signing — the
/// receiver re-derives the canonical bytes from the received
/// fields and verifies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SignedSharePayload {
    /// The sending memory node's friendly name (the
    /// `[controller] name` of the source memory node). Useful
    /// for the chronicle audit trail; not used by the
    /// signature math.
    pub source_node: String,
    /// The agent name on the source side. The record's
    /// `source` column must match (the recv side's
    /// TrustChecker re-checks).
    pub source_agent: String,
    /// The target agent (the receiver). This becomes the
    /// `source` column on the copied row.
    pub target_agent: String,
    /// The full `MemoryRecord` being shared. The receiver
    /// re-builds the copy via `build_copy` after verifying.
    pub record: MemoryRecord,
    /// Optional operator-supplied note (lands as a
    /// `share_note:<...>` tag on the receiver's copy).
    #[serde(default)]
    pub message: Option<String>,
    /// Ed25519 signature of the canonical bytes.
    /// Base64-encoded so the JSON wire stays printable.
    pub signature: String,
    /// Ed25519 verifying key (the source node's public key).
    /// Base64-encoded.
    pub source_pubkey: String,
}

impl SignedSharePayload {
    /// Build a signed payload using `signer` to sign the
    /// canonical bytes. The verifying key is derived from the
    /// signing key and serialised into the payload alongside
    /// the signature.
    pub fn sign(
        signer: &SigningKey,
        source_node: impl Into<String>,
        source_agent: impl Into<String>,
        target_agent: impl Into<String>,
        record: MemoryRecord,
        message: Option<String>,
    ) -> Self {
        let source_node = source_node.into();
        let source_agent = source_agent.into();
        let target_agent = target_agent.into();
        let canonical = canonical_bytes(&source_node, &source_agent, &target_agent, &record);
        let signature: Signature = signer.sign(&canonical);
        use base64::Engine;
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signature.to_bytes());
        let pubkey_b64 =
            base64::engine::general_purpose::STANDARD.encode(signer.verifying_key().to_bytes());
        Self {
            source_node,
            source_agent,
            target_agent,
            record,
            message,
            signature: sig_b64,
            source_pubkey: pubkey_b64,
        }
    }

    /// Verify the signature on this payload. Returns
    /// `Err(RejectReason::InvalidSignature{..})` when the
    /// signature can't be decoded, the pubkey is malformed,
    /// or the signature doesn't validate the canonical
    /// bytes against the claimed pubkey.
    pub fn verify(&self) -> Result<VerifyingKey, RejectReason> {
        use base64::Engine;
        let pubkey_bytes = base64::engine::general_purpose::STANDARD
            .decode(self.source_pubkey.as_bytes())
            .map_err(|e| RejectReason::InvalidSignature {
                detail: format!("source_pubkey base64 decode: {e}"),
            })?;
        if pubkey_bytes.len() != 32 {
            return Err(RejectReason::InvalidSignature {
                detail: format!(
                    "source_pubkey wrong length: {got}, want 32",
                    got = pubkey_bytes.len()
                ),
            });
        }
        let mut pubkey_arr = [0u8; 32];
        pubkey_arr.copy_from_slice(&pubkey_bytes);
        let verifying =
            VerifyingKey::from_bytes(&pubkey_arr).map_err(|e| RejectReason::InvalidSignature {
                detail: format!("source_pubkey parse: {e}"),
            })?;
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(self.signature.as_bytes())
            .map_err(|e| RejectReason::InvalidSignature {
                detail: format!("signature base64 decode: {e}"),
            })?;
        if sig_bytes.len() != 64 {
            return Err(RejectReason::InvalidSignature {
                detail: format!(
                    "signature wrong length: {got}, want 64",
                    got = sig_bytes.len()
                ),
            });
        }
        let mut sig_arr = [0u8; 64];
        sig_arr.copy_from_slice(&sig_bytes);
        let signature = Signature::from_bytes(&sig_arr);
        let canonical = canonical_bytes(
            &self.source_node,
            &self.source_agent,
            &self.target_agent,
            &self.record,
        );
        verifying
            .verify(&canonical, &signature)
            .map_err(|e| RejectReason::InvalidSignature {
                detail: format!("signature mismatch: {e}"),
            })?;
        Ok(verifying)
    }
}

/// Build the byte sequence the signature covers. Length-
/// prefix every field so a `|`-bearing source_agent can't
/// collide with a target_agent (sign-extension attack
/// surface).
fn canonical_bytes(
    source_node: &str,
    source_agent: &str,
    target_agent: &str,
    record: &MemoryRecord,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        source_node.len() + source_agent.len() + target_agent.len() + record.text.len() + 64,
    );
    push_prefixed(&mut out, b"sn", source_node.as_bytes());
    push_prefixed(&mut out, b"sa", source_agent.as_bytes());
    push_prefixed(&mut out, b"ta", target_agent.as_bytes());
    push_prefixed(&mut out, b"rid", record.id.as_bytes());
    push_prefixed(&mut out, b"rtx", record.text.as_bytes());
    push_prefixed(&mut out, b"rly", record.layer.as_str().as_bytes());
    push_prefixed(&mut out, b"rsc", record.source.as_bytes());
    // Tags are serialised as a JSON array of strings so the
    // sender + receiver agree on canonical form even when
    // tag order differs across runs.
    let mut tags = record.tags.clone();
    tags.sort();
    let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into());
    push_prefixed(&mut out, b"rtg", tags_json.as_bytes());
    // SECTION 9: cover every security-relevant field so a MITM
    // that flips, e.g., `shareable` false→true or `tenant_id` on
    // a poison-tagged row breaks the signature. Order is fixed.
    push_prefixed(&mut out, b"vfr", &record.valid_from.to_le_bytes());
    push_opt_i64(&mut out, b"vto", record.valid_to);
    push_prefixed(&mut out, b"shr", &[record.shareable as u8]);
    let mut shared = record.shared_with.clone();
    shared.sort();
    let shared_json = serde_json::to_string(&shared).unwrap_or_else(|_| "[]".into());
    push_prefixed(&mut out, b"swt", shared_json.as_bytes());
    push_prefixed(&mut out, b"spo", record.share_policy.as_str().as_bytes());
    push_prefixed(&mut out, b"sst", record.source_trust.as_str().as_bytes());
    push_prefixed(&mut out, b"frz", &[record.frozen as u8]);
    push_prefixed(&mut out, b"con", &[record.consolidated as u8]);
    push_opt_str(&mut out, b"tid", record.tenant_id.as_deref());
    push_opt_str(&mut out, b"sby", record.superseded_by.as_deref());
    out
}

fn push_prefixed(out: &mut Vec<u8>, label: &[u8], value: &[u8]) {
    out.extend_from_slice(label);
    out.push(b':');
    let len = (value.len() as u32).to_le_bytes();
    out.extend_from_slice(&len);
    out.extend_from_slice(value);
    out.push(b'\n');
}

/// SECTION 9: encode an optional string with an explicit
/// presence byte (0 = None, 1 = Some) so `None` and `Some("")`
/// are distinct in the signed bytes.
fn push_opt_str(out: &mut Vec<u8>, label: &[u8], value: Option<&str>) {
    let mut buf = Vec::new();
    match value {
        None => buf.push(0u8),
        Some(s) => {
            buf.push(1u8);
            buf.extend_from_slice(s.as_bytes());
        }
    }
    push_prefixed(out, label, &buf);
}

/// SECTION 9: encode an optional i64 with a presence byte.
fn push_opt_i64(out: &mut Vec<u8>, label: &[u8], value: Option<i64>) {
    let mut buf = Vec::new();
    match value {
        None => buf.push(0u8),
        Some(n) => {
            buf.push(1u8);
            buf.extend_from_slice(&n.to_le_bytes());
        }
    }
    push_prefixed(out, label, &buf);
}

/// Error envelope returned by [`RemoteKnowledgeDispatcher`].
#[derive(Clone, Debug, thiserror::Error)]
pub enum RemoteShareError {
    #[error("knowledge.accept_shared: unreachable node {node}: {detail}")]
    Unreachable { node: String, detail: String },
    #[error("knowledge.accept_shared: receiver rejected: {detail}")]
    Rejected {
        detail: String,
        reason: RejectReason,
    },
    #[error("knowledge.accept_shared: transport: {0}")]
    Transport(String),
}

/// Cross-node knowledge dispatcher. The
/// `KnowledgeService` holds an
/// `Arc<dyn RemoteKnowledgeDispatcher>` so unit tests can
/// stub it without libp2p; production wires it to a
/// `MeshClient`-backed impl built in `controller_runtime`.
pub trait RemoteKnowledgeDispatcher: Send + Sync {
    fn accept_shared(
        &self,
        node: String,
        payload: SignedSharePayload,
    ) -> BoxFuture<'static, Result<(), RemoteShareError>>;
}

/// A null dispatcher that rejects every cross-node call with
/// `Unreachable`. Used when the controller wasn't wired with
/// a mesh client (e.g. unit tests).
#[derive(Clone, Default)]
pub struct NullRemoteDispatcher;

impl RemoteKnowledgeDispatcher for NullRemoteDispatcher {
    fn accept_shared(
        &self,
        node: String,
        _payload: SignedSharePayload,
    ) -> BoxFuture<'static, Result<(), RemoteShareError>> {
        Box::pin(async move {
            Err(RemoteShareError::Unreachable {
                node,
                detail: "no remote dispatcher configured".to_string(),
            })
        })
    }
}

/// RELIX-7.16 GAP 3 — a dispatcher that delegates to a
/// `tokio::sync::OnceCell` once it's populated. This lets
/// `KnowledgeService` carry a non-`None` `remote` field from
/// construction time while the actual mesh-backed dispatcher
/// gets wired post-rpc::Client startup. Until the cell is
/// populated every `accept_shared` call rejects with
/// `Unreachable { detail: "dispatcher not yet wired" }`.
pub struct LateBoundDispatcher {
    cell: Arc<tokio::sync::OnceCell<Arc<dyn RemoteKnowledgeDispatcher>>>,
}

impl LateBoundDispatcher {
    pub fn new(cell: Arc<tokio::sync::OnceCell<Arc<dyn RemoteKnowledgeDispatcher>>>) -> Self {
        Self { cell }
    }
}

impl RemoteKnowledgeDispatcher for LateBoundDispatcher {
    fn accept_shared(
        &self,
        node: String,
        payload: SignedSharePayload,
    ) -> BoxFuture<'static, Result<(), RemoteShareError>> {
        let cell = self.cell.clone();
        Box::pin(async move {
            let Some(inner) = cell.get() else {
                return Err(RemoteShareError::Unreachable {
                    node,
                    detail: "knowledge mesh dispatcher not yet wired".into(),
                });
            };
            inner.accept_shared(node, payload).await
        })
    }
}

/// RELIX-7.16 GAP 3 production dispatcher — calls
/// `knowledge.accept_shared` on a remote peer via libp2p. One
/// instance carries a single `MeshClient`; the wiring layer
/// maps `node_name -> MeshKnowledgeDispatcher` and packs the
/// per-node lookups into a [`MeshKnowledgeRouter`] so the
/// service-side trait only has to know about one dispatcher.
pub struct MeshKnowledgeDispatcher {
    mesh: crate::manifest::MeshClient,
    /// Peer alias to dial (`mesh.call(&alias, envelope)`).
    /// Operators configure aliases under `[peers]` in the
    /// controller TOML; this is the same alias.
    alias: String,
    /// Identity bundle used to authenticate the outbound RPC at
    /// the transport layer.
    identity: relix_core::bundle::Bundle,
    deadline_secs: i64,
}

impl MeshKnowledgeDispatcher {
    pub fn new(
        mesh: crate::manifest::MeshClient,
        alias: impl Into<String>,
        identity: relix_core::bundle::Bundle,
        deadline_secs: i64,
    ) -> Self {
        Self {
            mesh,
            alias: alias.into(),
            identity,
            deadline_secs,
        }
    }
}

impl RemoteKnowledgeDispatcher for MeshKnowledgeDispatcher {
    fn accept_shared(
        &self,
        node: String,
        payload: SignedSharePayload,
    ) -> BoxFuture<'static, Result<(), RemoteShareError>> {
        use crate::dispatch::{build_request, decode_response};
        use crate::transport::envelope::ResponseResult;
        let mesh = self.mesh.clone();
        let alias = self.alias.clone();
        let identity = self.identity.clone();
        let deadline = self.deadline_secs;
        Box::pin(async move {
            let args = match serde_json::to_vec(&payload) {
                Ok(b) => b,
                Err(e) => {
                    return Err(RemoteShareError::Transport(format!(
                        "encode SignedSharePayload: {e}"
                    )));
                }
            };
            let envelope = build_request("knowledge.accept_shared", args, identity, deadline);
            let resp_bytes = match mesh.call(&alias, envelope).await {
                Ok(b) => b,
                Err(e) => {
                    return Err(RemoteShareError::Unreachable {
                        node,
                        detail: format!("mesh.call({alias}): {e}"),
                    });
                }
            };
            let resp = match decode_response(&resp_bytes) {
                Ok(r) => r,
                Err(e) => {
                    return Err(RemoteShareError::Transport(format!("decode_response: {e}")));
                }
            };
            match resp.res {
                ResponseResult::Ok(_) => Ok(()),
                ResponseResult::Err(env) => Err(RemoteShareError::Transport(format!(
                    "receiver returned err {kind}: {cause}",
                    kind = env.kind,
                    cause = env.cause
                ))),
                ResponseResult::StreamHandle(_) => Err(RemoteShareError::Transport(
                    "receiver returned stream handle for unary cap".into(),
                )),
            }
        })
    }
}

/// Routes per-node `accept_shared` calls to the right inner
/// [`MeshKnowledgeDispatcher`]. The controller_runtime builds
/// this once at boot from `[peers]` (one inner per peer alias
/// === node name).
pub struct MeshKnowledgeRouter {
    by_node: BTreeMap<String, Arc<MeshKnowledgeDispatcher>>,
}

impl MeshKnowledgeRouter {
    pub fn new(by_node: BTreeMap<String, Arc<MeshKnowledgeDispatcher>>) -> Self {
        Self { by_node }
    }
}

impl RemoteKnowledgeDispatcher for MeshKnowledgeRouter {
    fn accept_shared(
        &self,
        node: String,
        payload: SignedSharePayload,
    ) -> BoxFuture<'static, Result<(), RemoteShareError>> {
        let dispatcher = self.by_node.get(&node).cloned();
        Box::pin(async move {
            let Some(d) = dispatcher else {
                return Err(RemoteShareError::Unreachable {
                    node,
                    detail: "no MeshKnowledgeDispatcher configured for this node".into(),
                });
            };
            d.accept_shared(node, payload).await
        })
    }
}

/// In-memory dispatcher used by tests: maps `node ->
/// destination LayeredMemoryStore`. The dispatcher signs +
/// the destination's accept handler verifies + inserts —
/// exercising the full local→remote loop without libp2p.
#[derive(Clone, Default)]
pub struct InMemoryRemoteDispatcher {
    /// Per-node service that handles the incoming
    /// `accept_shared` call. The service runs its own
    /// TrustChecker against the destination store.
    inner: BTreeMap<String, Arc<super::service::KnowledgeService>>,
    /// Nodes that should always fail. Used to test the
    /// `Unreachable` path.
    unreachable: std::collections::BTreeSet<String>,
}

impl InMemoryRemoteDispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_node(
        mut self,
        node: impl Into<String>,
        service: Arc<super::service::KnowledgeService>,
    ) -> Self {
        self.inner.insert(node.into(), service);
        self
    }

    pub fn with_unreachable(mut self, node: impl Into<String>) -> Self {
        self.unreachable.insert(node.into());
        self
    }
}

impl RemoteKnowledgeDispatcher for InMemoryRemoteDispatcher {
    fn accept_shared(
        &self,
        node: String,
        payload: SignedSharePayload,
    ) -> BoxFuture<'static, Result<(), RemoteShareError>> {
        if self.unreachable.contains(&node) {
            return Box::pin(async move {
                Err(RemoteShareError::Unreachable {
                    node,
                    detail: "test fixture marked node unreachable".into(),
                })
            });
        }
        let service = self.inner.get(&node).cloned();
        Box::pin(async move {
            let Some(service) = service else {
                return Err(RemoteShareError::Unreachable {
                    node,
                    detail: "no service registered for node".into(),
                });
            };
            service.accept_shared(payload).map_err(|e| match e {
                super::service::ShareError::InvalidArgs(m) => RemoteShareError::Transport(m),
                super::service::ShareError::Store(s) => RemoteShareError::Transport(s.to_string()),
                super::service::ShareError::Rejected(reason) => RemoteShareError::Rejected {
                    detail: format!("{reason:?}"),
                    reason,
                },
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::memory::schema::{MemoryLayer, MemoryRecord};
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn obs(id: &str, owner: &str, text: &str) -> MemoryRecord {
        let mut r = MemoryRecord::new_raw(id, text, owner);
        r.layer = MemoryLayer::Observation;
        r.shareable = true;
        r
    }

    #[test]
    fn sign_then_verify_round_trips_successfully() {
        let signer = SigningKey::generate(&mut OsRng);
        let payload = SignedSharePayload::sign(
            &signer,
            "memory-node-1",
            "alice",
            "bob",
            obs("a1", "alice", "fact one"),
            Some("worth keeping".into()),
        );
        let pk = payload.verify().expect("verify succeeds");
        assert_eq!(
            pk.to_bytes(),
            signer.verifying_key().to_bytes(),
            "verify returns the same pubkey"
        );
    }

    #[test]
    fn tampering_with_record_text_breaks_signature() {
        let signer = SigningKey::generate(&mut OsRng);
        let mut payload = SignedSharePayload::sign(
            &signer,
            "node-1",
            "alice",
            "bob",
            obs("a1", "alice", "fact one"),
            None,
        );
        // Operator-in-the-middle flips the record text after
        // signing → verify must fail.
        payload.record.text = "fact one MUTATED".into();
        let err = payload.verify().unwrap_err();
        assert!(matches!(err, RejectReason::InvalidSignature { .. }));
    }

    #[test]
    fn tampering_with_target_agent_breaks_signature() {
        let signer = SigningKey::generate(&mut OsRng);
        let mut payload = SignedSharePayload::sign(
            &signer,
            "node-1",
            "alice",
            "bob",
            obs("a1", "alice", "fact one"),
            None,
        );
        payload.target_agent = "mallory".into();
        let err = payload.verify().unwrap_err();
        assert!(matches!(err, RejectReason::InvalidSignature { .. }));
    }

    #[test]
    fn corrupted_signature_base64_yields_invalid_signature() {
        let signer = SigningKey::generate(&mut OsRng);
        let mut payload =
            SignedSharePayload::sign(&signer, "n", "a", "b", obs("a1", "a", "t"), None);
        payload.signature = "@@@@".into();
        match payload.verify().unwrap_err() {
            RejectReason::InvalidSignature { detail } => {
                assert!(
                    detail.contains("base64"),
                    "expected base64 mention in detail: {detail}"
                );
            }
            o => panic!("expected InvalidSignature, got {o:?}"),
        }
    }

    #[test]
    fn pubkey_mismatch_breaks_verify() {
        let signer = SigningKey::generate(&mut OsRng);
        let other = SigningKey::generate(&mut OsRng);
        let mut payload =
            SignedSharePayload::sign(&signer, "n", "a", "b", obs("a1", "a", "t"), None);
        use base64::Engine;
        payload.source_pubkey =
            base64::engine::general_purpose::STANDARD.encode(other.verifying_key().to_bytes());
        match payload.verify().unwrap_err() {
            RejectReason::InvalidSignature { .. } => {}
            o => panic!("expected InvalidSignature, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn null_dispatcher_always_returns_unreachable() {
        let d = NullRemoteDispatcher;
        let signer = SigningKey::generate(&mut OsRng);
        let p = SignedSharePayload::sign(&signer, "src", "a", "b", obs("a1", "a", "t"), None);
        let err = d.accept_shared("node-1".into(), p).await.unwrap_err();
        match err {
            RemoteShareError::Unreachable { node, .. } => assert_eq!(node, "node-1"),
            o => panic!("expected Unreachable, got {o:?}"),
        }
    }
}
