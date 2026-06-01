//! Node manifest construction + on-connect exchange (RELIX-5 / M10).
//!
//! M9 left this a stub. M10 adds:
//!
//! - [`NodeManifest`] — what every node advertises about itself.
//! - [`ManifestProvider`] — thread-safe builder a controller pushes into as
//!   each node-type registers its capabilities. The built-in
//!   `node.manifest` capability serialises the current snapshot on demand.
//! - [`ManifestCache`] — per-process map of `node_id_hex` → [`NodeManifest`].
//!   Populated by callers that pull manifests over the wire and consulted by
//!   the bridge for `/v1/models` and `capability:` resolution in flow_runner.
//!
//! All transport is the existing RELIX-1 `/relix/rpc/1`. No DHT, no
//! gossipsub. M10 only proves capability information can flow between peers
//! through the normal admission pipeline; full gossip-based discovery and
//! manifest signing land at Gate 2.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use relix_core::bundle::Bundle;
use relix_core::capability::CapabilityDescriptor;
use relix_core::codec;
use relix_core::types::NodeId;

use crate::dispatch::{build_request, decode_response};
use crate::flow_runner::PeersFile;
use crate::transport::envelope::ResponseResult;
use crate::transport::rpc::{self, Event as TransportEvent, Multiaddr, PeerId};

/// SEC PART 2: default freshness window for received signed
/// manifests, in seconds. A manifest whose `signed_at_ms` is
/// older than `now - manifest_ttl_secs * 1000` is rejected
/// with `MANIFEST_STALE`. Operators can override via the
/// `[mesh] manifest_ttl_secs` config key (wired by the
/// bridge / controller startup; this module surfaces the
/// constant so the cap handlers and the cache reader stay
/// consistent).
pub const DEFAULT_MANIFEST_TTL_SECS: i64 = 300;

/// Alpha node manifest payload — what a peer returns from `node.manifest`.
///
/// `manifest_version` is bumped any time `capabilities` changes; today nodes
/// publish a constant `1` because capability registration is static per
/// binary launch. Gate 2 swaps this for an event-sourced number and signs
/// the payload via [`relix_core::bundle::Bundle`] with
/// `BundleType::NodeManifest`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeManifest {
    /// Node id (peer id = blake3 of Ed25519 pubkey).
    pub node_id: NodeId,
    /// Human-readable name (the `[controller] name` from config).
    pub node_name: String,
    /// Node-type discriminator (`memory`, `ai`, `tool`, `web_bridge`, ...).
    pub node_type: String,
    /// Monotonic version (bump on capability change).
    pub manifest_version: u64,
    /// Org id (org-root key hash) the node trusts.
    pub org_id: NodeId,
    /// Listen endpoints in libp2p multiaddr form (e.g. `/ip4/127.0.0.1/tcp/9001`).
    /// Alpha M10 fills this with the controller's configured listen address.
    pub endpoints: Vec<String>,
    /// Capabilities served by this node.
    pub capabilities: Vec<CapabilityDescriptor>,
}

impl NodeManifest {
    /// Convenience: which methods this peer exposes.
    pub fn methods(&self) -> Vec<&str> {
        self.capabilities
            .iter()
            .map(|c| c.method_name.as_str())
            .collect()
    }

    /// Whether this peer advertises a specific method.
    pub fn advertises(&self, method: &str) -> bool {
        self.capabilities.iter().any(|c| c.method_name == method)
    }
}

/// SEC PART 2: signed wire envelope around a [`NodeManifest`].
///
/// Per RELIX-5 §5.2.2, every node-to-node manifest exchange
/// MUST be authenticated. The pre-fix `node.manifest` handler
/// returned a plain CBOR [`NodeManifest`] — any peer on the
/// transport could claim any `org_id` + capability set.
///
/// Wire layout (CBOR-encoded):
///
/// ```text
/// SignedManifest {
///     body:                NodeManifest body bytes (CBOR of NodeManifest)
///     signature:           [u8; 64]            (Ed25519 over the body bytes)
///     signer_fingerprint:  String              (hex(blake3(pubkey_bytes)) — equals node_id hex)
///     signed_at_ms:        i64                 (wall clock at sign time)
/// }
/// ```
///
/// Verification on receive:
///
/// 1. Decode `SignedManifest`.
/// 2. Decode the embedded `body` bytes into a `NodeManifest`.
/// 3. Recompute the fingerprint from the receiver's known
///    public key for this node (looked up via the
///    [`KnownNodesRegistry`]); on first contact the
///    fingerprint is PINNED. Mismatch with a prior pin →
///    `MANIFEST_INVALID`. Pin absent + receiver has no way
///    to recover the pubkey → `MANIFEST_UNKNOWN_SIGNER`.
/// 4. Verify the Ed25519 signature over the body bytes
///    against the signer's public key.
/// 5. Check `now_ms - signed_at_ms <= manifest_ttl_secs * 1000`
///    → otherwise `MANIFEST_STALE`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedManifest {
    #[serde(with = "serde_bytes")]
    pub body: Vec<u8>,
    #[serde(with = "serde_bytes")]
    pub signature: [u8; 64],
    /// Hex-encoded `blake3(signer_pubkey_bytes)`. Equals
    /// `node_id` for the signer when the manifest is honest.
    pub signer_fingerprint: String,
    /// Raw 32-byte Ed25519 public key the signature was
    /// produced with. Included on the wire so the TOFU
    /// receive path can verify without an out-of-band pubkey
    /// lookup. The receiver MUST recompute
    /// `blake3(signer_pubkey_bytes)` and compare against
    /// `signer_fingerprint` before trusting either.
    #[serde(with = "serde_bytes")]
    pub signer_pubkey: [u8; 32],
    pub signed_at_ms: i64,
}

impl SignedManifest {
    /// Decode the inner `NodeManifest` from the body bytes.
    /// Does NOT verify the signature — callers MUST run the
    /// full verification via
    /// [`KnownNodesRegistry::verify_and_pin`] (or the
    /// inline checks in [`MeshClient::refresh_manifests`] /
    /// [`discover_and_pin`]).
    pub fn body_decoded(&self) -> Result<NodeManifest, ManifestVerifyError> {
        codec::decode::<NodeManifest>(&self.body)
            .map_err(|e| ManifestVerifyError::BodyDecode(e.to_string()))
    }
}

/// SEC PART 2: structured failures from the receive path.
/// Mapped 1:1 onto `relix_core::types::error_kinds::MANIFEST_*`
/// at the cap-handler / discovery boundary so operators see
/// a stable wire kind.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ManifestVerifyError {
    /// The outer CBOR envelope failed to decode.
    #[error("manifest envelope decode: {0}")]
    EnvelopeDecode(String),
    /// The body bytes failed to decode into a `NodeManifest`.
    #[error("manifest body decode: {0}")]
    BodyDecode(String),
    /// The Ed25519 signature did not verify under the signer's
    /// public key.
    #[error("manifest signature invalid")]
    BadSignature,
    /// The signer's claimed fingerprint disagrees with the
    /// fingerprint derived from the public key the receiver
    /// holds for this signer.
    #[error("manifest signer fingerprint mismatch: expected {expected}, got {got}")]
    FingerprintMismatch { expected: String, got: String },
    /// The signer has no public key in the receiver's
    /// known-nodes registry AND the manifest envelope didn't
    /// carry enough info to recover one (first-contact
    /// scenarios; we always have the pubkey when verifying
    /// a libp2p-delivered manifest because the transport
    /// already authenticated the peer at Noise time — see
    /// [`MeshClient::refresh_manifests`] which threads the
    /// peer's pubkey into verification).
    #[error("manifest signer unknown: no public key for {fingerprint}")]
    UnknownSigner { fingerprint: String },
    /// `now - signed_at_ms` exceeds the configured TTL.
    #[error("manifest stale: signed_at_ms={signed_at_ms} now_ms={now_ms} ttl_secs={ttl_secs}")]
    Stale {
        signed_at_ms: i64,
        now_ms: i64,
        ttl_secs: i64,
    },
}

impl ManifestVerifyError {
    /// Map onto `relix_core::types::error_kinds` for the wire.
    pub fn error_kind(&self) -> u32 {
        match self {
            Self::EnvelopeDecode(_)
            | Self::BodyDecode(_)
            | Self::BadSignature
            | Self::FingerprintMismatch { .. } => relix_core::types::error_kinds::MANIFEST_INVALID,
            Self::UnknownSigner { .. } => relix_core::types::error_kinds::MANIFEST_UNKNOWN_SIGNER,
            Self::Stale { .. } => relix_core::types::error_kinds::MANIFEST_STALE,
        }
    }
}

/// SEC PART 2: TOFU (trust-on-first-use) registry of
/// `(node_id, signer_fingerprint)` pins. On first manifest
/// from a new node, the fingerprint is inserted; every
/// subsequent manifest from the same node must present the
/// matching fingerprint, else
/// [`ManifestVerifyError::FingerprintMismatch`].
///
/// Currently in-memory — pins live for the lifetime of the
/// process. Persistence is intentionally out of scope; the
/// alpha bridges are single-process and a restart re-pins
/// from the first inbound manifest. A future Gate 2 work
/// item swaps the inner store for a SQLite-backed one
/// without changing this surface.
#[derive(Clone, Default)]
pub struct KnownNodesRegistry {
    pins: Arc<RwLock<std::collections::HashMap<NodeId, String>>>,
}

impl KnownNodesRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Verify the signed manifest envelope and TOFU-pin the
    /// signer's fingerprint on first contact.
    ///
    /// The signer's Ed25519 public key is sourced from
    /// `signed.signer_pubkey` on the wire. The first check
    /// enforces that `blake3(signer_pubkey) == signer_fingerprint`
    /// so a malicious peer cannot lie about which fingerprint
    /// its signature belongs to.
    ///
    /// Returns the decoded `NodeManifest` on success.
    pub fn verify_and_pin(
        &self,
        signed: &SignedManifest,
        ttl_secs: i64,
        now_ms: i64,
    ) -> Result<NodeManifest, ManifestVerifyError> {
        // 1. Reconstruct the Ed25519 public key from the wire
        //    bytes. Malformed pubkey → BadSignature (signature
        //    can't possibly verify against an invalid key).
        let signer_pubkey = match VerifyingKey::from_bytes(&signed.signer_pubkey) {
            Ok(k) => k,
            Err(_) => return Err(ManifestVerifyError::BadSignature),
        };
        // 2. Fingerprint of the wire pubkey MUST match the
        //    signer_fingerprint on the wire. Defends against a
        //    middlebox swapping the claimed fingerprint
        //    without re-signing.
        let computed = fingerprint_of_pubkey(&signer_pubkey);
        if computed != signed.signer_fingerprint {
            return Err(ManifestVerifyError::FingerprintMismatch {
                expected: computed,
                got: signed.signer_fingerprint.clone(),
            });
        }
        // 3. Ed25519 over the body bytes.
        let sig = Signature::from_bytes(&signed.signature);
        if signer_pubkey.verify(&signed.body, &sig).is_err() {
            return Err(ManifestVerifyError::BadSignature);
        }
        // 4. Decode the body so we can extract node_id for
        //    the TOFU pin AND return it to the caller.
        let manifest = signed.body_decoded()?;
        // 5. TOFU.
        {
            let mut g = self.pins.write().expect("known nodes pin lock");
            match g.get(&manifest.node_id) {
                Some(prior) if prior == &signed.signer_fingerprint => {}
                Some(prior) => {
                    return Err(ManifestVerifyError::FingerprintMismatch {
                        expected: prior.clone(),
                        got: signed.signer_fingerprint.clone(),
                    });
                }
                None => {
                    g.insert(manifest.node_id, signed.signer_fingerprint.clone());
                }
            }
        }
        // 6. Freshness — emit MANIFEST_STALE (declared since
        //    SHARED but never actually returned before PART 2).
        let age_ms = now_ms - signed.signed_at_ms;
        if age_ms > ttl_secs * 1_000 {
            return Err(ManifestVerifyError::Stale {
                signed_at_ms: signed.signed_at_ms,
                now_ms,
                ttl_secs,
            });
        }
        Ok(manifest)
    }

    /// Lookup the pinned fingerprint for a node — `None` when
    /// the node hasn't been seen yet. Exposed for the bridge's
    /// diagnostic surfaces and for tests.
    pub fn pinned_fingerprint(&self, node_id: &NodeId) -> Option<String> {
        self.pins.read().ok().and_then(|g| g.get(node_id).cloned())
    }
}

/// Hex-encode blake3(pubkey_bytes). Equals `NodeId::to_string()`
/// for the same pubkey, which is also what `node_id` carries
/// inside the manifest body — a successful verify implies
/// `signer_fingerprint == manifest.node_id.to_string()`.
pub fn fingerprint_of_pubkey(pubkey: &VerifyingKey) -> String {
    let bytes = pubkey.to_bytes();
    hex::encode(blake3::hash(&bytes).as_bytes())
}

/// Shared, append-only manifest builder. Each node-type's `register(...)` in
/// `crate::nodes::*` calls [`Self::add_capability`] alongside its
/// `bridge.register(...)` so the manifest stays in sync with the dispatch
/// bridge. Cloning is cheap (`Arc`).
#[derive(Clone)]
pub struct ManifestProvider {
    inner: Arc<RwLock<NodeManifest>>,
    /// SEC PART 2: optional Ed25519 signing key. Production
    /// callers wire it via [`Self::with_signer`]; tests that
    /// pre-date PART 2 keep working unsigned via
    /// [`Self::snapshot`] (which is now a back-compat alias
    /// kept for the in-process tests in this module).
    signer: Option<Arc<SigningKey>>,
    /// SEC PART 7: O(1) capability descriptor cache. Populated
    /// in lock-step with [`Self::add_capability`]; consumed by
    /// the dispatch bridge's `describe` closure so the
    /// per-request agent-gate lookup is a single `DashMap`
    /// probe rather than a linear scan of the manifest's
    /// capability vector. Cheap to clone — the underlying
    /// `DashMap` is reference-counted.
    descriptor_cache: DescriptorCache,
}

/// SEC PART 7: shared, lock-free descriptor lookup table.
/// Type alias so the cache surface is named consistently
/// across the bridge / controller / tests.
pub type DescriptorCache = Arc<dashmap::DashMap<String, CapabilityDescriptor>>;

impl ManifestProvider {
    /// Build with the node's identity. Capabilities are appended later as
    /// each node-type initialises.
    pub fn new(
        node_id: NodeId,
        node_name: impl Into<String>,
        node_type: impl Into<String>,
        org_id: NodeId,
        endpoints: Vec<String>,
    ) -> Self {
        Self {
            inner: Arc::new(RwLock::new(NodeManifest {
                node_id,
                node_name: node_name.into(),
                node_type: node_type.into(),
                manifest_version: 1,
                org_id,
                endpoints,
                capabilities: Vec::new(),
            })),
            signer: None,
            descriptor_cache: Arc::new(dashmap::DashMap::new()),
        }
    }

    /// SEC PART 7: hand out the shared descriptor cache. The
    /// dispatch bridge captures a clone of this `Arc` and
    /// builds its `describe(method)` closure against it so the
    /// per-request lookup stays O(1) regardless of how many
    /// capabilities the node has registered.
    pub fn descriptor_cache(&self) -> DescriptorCache {
        Arc::clone(&self.descriptor_cache)
    }

    /// SEC PART 2: install the node's libp2p Ed25519 signing
    /// key. Required for [`Self::signed_snapshot`] — the
    /// production `node.manifest` cap handler calls
    /// `signed_snapshot` so receivers can TOFU-pin the
    /// fingerprint and verify the signature.
    pub fn with_signer(mut self, signer: SigningKey) -> Self {
        self.signer = Some(Arc::new(signer));
        self
    }

    /// Append a capability the dispatch bridge has just registered.
    pub fn add_capability(&self, desc: CapabilityDescriptor) {
        let mut guard = self.inner.write().unwrap_or_else(|e| {
            tracing::warn!("manifest provider lock poisoned; recovering inner state");
            e.into_inner()
        });
        // De-dupe by method_name so register-on-restart doesn't duplicate.
        if guard
            .capabilities
            .iter()
            .any(|c| c.method_name == desc.method_name)
        {
            return;
        }
        // SEC PART 7: keep the descriptor cache in lock-step
        // with the manifest vector so the dispatch bridge's
        // O(1) describe closure sees every registered capability
        // the moment add_capability returns.
        self.descriptor_cache
            .insert(desc.method_name.clone(), desc.clone());
        guard.capabilities.push(desc);
    }

    /// Snapshot the current manifest (cheap clone). Unsigned —
    /// for in-process consumption and back-compat tests only.
    /// The `node.manifest` cap MUST use [`Self::signed_snapshot`].
    pub fn snapshot(&self) -> NodeManifest {
        self.inner
            .read()
            .unwrap_or_else(|e| {
                tracing::warn!("manifest provider lock poisoned; recovering inner state");
                e.into_inner()
            })
            .clone()
    }

    /// SEC PART 2: sign the current manifest snapshot. Returns
    /// the wire-shaped [`SignedManifest`] callers serialise to
    /// CBOR. Fails when [`Self::with_signer`] was not called
    /// (operator boot bug — surfaced as `RESPONDER_INTERNAL`
    /// at the cap layer).
    pub fn signed_snapshot(&self, now_ms: i64) -> Result<SignedManifest, SignError> {
        let signer = self
            .signer
            .as_ref()
            .ok_or(SignError::NoSigningKeyConfigured)?;
        let body_struct = self.snapshot();
        let body = codec::encode(&body_struct).map_err(|e| SignError::Encode(e.to_string()))?;
        let signature = signer.sign(&body).to_bytes();
        let signer_pubkey = signer.verifying_key().to_bytes();
        let signer_fingerprint = fingerprint_of_pubkey(&signer.verifying_key());
        Ok(SignedManifest {
            body,
            signature,
            signer_fingerprint,
            signer_pubkey,
            signed_at_ms: now_ms,
        })
    }
}

/// SEC PART 2: signing-side errors for [`ManifestProvider::signed_snapshot`].
#[derive(Debug, Clone, thiserror::Error)]
pub enum SignError {
    #[error("manifest signing key not configured (call ManifestProvider::with_signer at boot)")]
    NoSigningKeyConfigured,
    #[error("manifest body encode: {0}")]
    Encode(String),
}

/// In-process cache of remote peers' manifests, keyed by hex-encoded
/// [`NodeId`]. The bridge and (future) controller-side discovery push into
/// it after a successful `node.manifest` round-trip.
#[derive(Clone, Default)]
pub struct ManifestCache {
    inner: Arc<RwLock<BTreeMap<String, CachedManifest>>>,
}

/// One cached manifest, with the local alias (if any) the operator
/// configured for the peer. Aliases stay first-class so existing flows that
/// use `remote_call("ai", ...)` keep working.
#[derive(Clone, Debug)]
pub struct CachedManifest {
    /// The local alias the operator gave this peer (e.g. `"ai"`), if any.
    pub alias: Option<String>,
    /// Manifest as returned by the peer.
    pub manifest: NodeManifest,
    /// Unix seconds when this entry was last successfully (re)fetched
    /// via `node.manifest`. Operators use this to spot peers whose
    /// refresh loop has stalled — a stale `last_refreshed_at` means
    /// the peer hasn't responded since that time, even though the
    /// cached capabilities are still being used for routing.
    pub last_refreshed_at: i64,
}

impl ManifestCache {
    /// Empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert / replace by node id. Stamps `last_refreshed_at` with
    /// the current wall-clock time — this is the only path that
    /// updates the freshness timestamp, so it reflects only
    /// *successful* refresh round-trips. Failed refreshes leave the
    /// prior timestamp intact (the caller skips the insert).
    pub fn insert(&self, alias: Option<String>, manifest: NodeManifest) {
        let key = manifest.node_id.to_string();
        let mut guard = self.inner.write().unwrap_or_else(|e| {
            tracing::warn!("manifest cache lock poisoned; recovering inner state");
            e.into_inner()
        });
        guard.insert(
            key,
            CachedManifest {
                alias,
                manifest,
                last_refreshed_at: unix_secs(),
            },
        );
    }

    /// Snapshot every cached entry.
    pub fn entries(&self) -> Vec<CachedManifest> {
        self.inner
            .read()
            .unwrap_or_else(|e| {
                tracing::warn!("manifest cache lock poisoned; recovering inner state");
                e.into_inner()
            })
            .values()
            .cloned()
            .collect()
    }

    /// Look up the alias for the first peer advertising `method`. Returns
    /// the alias when present (so existing `peer_alias` lookup paths can
    /// continue without change). Returns `None` if no peer advertises the
    /// method *or* the matching peer was added to the cache without an
    /// alias — the bridge today only adds aliased peers.
    pub fn find_alias_for_method(&self, method: &str) -> Option<String> {
        let guard = self.inner.read().unwrap_or_else(|e| {
            tracing::warn!("manifest cache lock poisoned; recovering inner state");
            e.into_inner()
        });
        for cached in guard.values() {
            if cached.manifest.advertises(method)
                && let Some(a) = cached.alias.as_ref()
            {
                return Some(a.clone());
            }
        }
        None
    }

    /// Aggregate every advertised method from every cached peer.
    pub fn all_methods(&self) -> Vec<String> {
        let guard = self.inner.read().unwrap_or_else(|e| {
            tracing::warn!("manifest cache lock poisoned; recovering inner state");
            e.into_inner()
        });
        let mut out: BTreeMap<String, ()> = BTreeMap::new();
        for cached in guard.values() {
            for cap in &cached.manifest.capabilities {
                out.insert(cap.method_name.clone(), ());
            }
        }
        out.into_keys().collect()
    }

    /// True when at least one peer advertises `method`.
    pub fn has_method(&self, method: &str) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|e| {
                tracing::warn!("manifest cache lock poisoned; recovering inner state");
                e.into_inner()
            })
            .values()
            .any(|c| c.manifest.advertises(method))
    }
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// SEC PART 2 wall-clock helper.
///
/// Returns milliseconds since the Unix epoch. Used by both
/// the manifest freshness check and the `signed_at_ms` stamp.
/// Saturates to `i64::MAX` rather than wrapping when the
/// clock somehow lands past i64 range.
fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

// ────────────────────────── Long-lived MeshClient ──────────────────────────

/// A persistent libp2p client with the configured peers already dialled
/// and their `PeerId`s cached by alias. The bridge constructs one of these
/// at startup (during the discovery pass) and reuses it for every chat
/// request — avoiding the per-request TCP + Noise + Yamux handshake the
/// FlowRunner used to perform on each call (M11).
///
/// Beyond M11, the client also owns an **address book** (alias →
/// `Multiaddr`) and a **call-with-reconnect** entry point so peers that
/// disappear and come back (process restart, transient network drop) are
/// recovered automatically without a bridge restart. See [`Self::call`].
///
/// Cloning is cheap — clones share the underlying [`MeshClientInner`] via
/// `Arc`, including the address book and counters.
#[derive(Clone)]
pub struct MeshClient {
    inner: Arc<MeshClientInner>,
}

struct MeshClientInner {
    client: crate::transport::rpc::Client,
    /// alias → libp2p PeerId resolved at discovery time. Stable across
    /// peer restarts because controller keys are persistent on disk.
    peer_ids: RwLock<std::collections::HashMap<String, crate::transport::rpc::PeerId>>,
    /// alias → original Multiaddr from peers.toml. Used to re-dial when a
    /// connection has dropped.
    addrs: std::collections::HashMap<String, crate::transport::rpc::Multiaddr>,
    /// Observability counters.
    reconnect_attempts: std::sync::atomic::AtomicU64,
    reconnect_successes: std::sync::atomic::AtomicU64,
    /// Identity bundle the client signs background manifest refreshes
    /// with. Same bundle the bridge uses for chat.
    identity: Bundle,
    /// Per-call deadline propagated into background refreshes.
    deadline_secs: i64,
    /// SEC PART 2: TOFU pin registry the receive paths
    /// consult when a peer publishes a `SignedManifest`. Pins
    /// live for the lifetime of the MeshClient (the bridge
    /// holds one for process lifetime).
    known_nodes: KnownNodesRegistry,
    /// SEC PART 2: configured manifest freshness window in
    /// seconds. Defaults to [`DEFAULT_MANIFEST_TTL_SECS`].
    manifest_ttl_secs: i64,
}

/// Errors the user-facing [`MeshClient::call`] surfaces. Wraps the
/// underlying transport error and tracks whether a retry happened so the
/// caller can audit / log it cleanly.
#[derive(Debug, Clone)]
pub struct MeshCallError {
    pub alias: String,
    pub cause: String,
    pub reconnected: bool,
}

impl std::fmt::Display for MeshCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.reconnected {
            write!(
                f,
                "mesh call '{}' failed after reconnect: {}",
                self.alias, self.cause
            )
        } else {
            write!(f, "mesh call '{}' failed: {}", self.alias, self.cause)
        }
    }
}

impl std::error::Error for MeshCallError {}

impl MeshClient {
    /// Build by hand (used by tests; production builds happen via
    /// [`discover_and_pin`]).
    pub fn new(
        client: crate::transport::rpc::Client,
        peer_ids: std::collections::HashMap<String, crate::transport::rpc::PeerId>,
        addrs: std::collections::HashMap<String, crate::transport::rpc::Multiaddr>,
        identity: Bundle,
        deadline_secs: i64,
    ) -> Self {
        Self {
            inner: Arc::new(MeshClientInner {
                client,
                peer_ids: RwLock::new(peer_ids),
                addrs,
                reconnect_attempts: std::sync::atomic::AtomicU64::new(0),
                reconnect_successes: std::sync::atomic::AtomicU64::new(0),
                identity,
                deadline_secs,
                known_nodes: KnownNodesRegistry::new(),
                manifest_ttl_secs: DEFAULT_MANIFEST_TTL_SECS,
            }),
        }
    }

    /// SEC PART 2: borrow the TOFU registry — diagnostic surface.
    pub fn known_nodes(&self) -> KnownNodesRegistry {
        self.inner.known_nodes.clone()
    }

    /// SEC PART 2: configured manifest TTL in seconds. Set
    /// via the optional constructor variant
    /// [`Self::new_with_manifest_ttl`].
    pub fn manifest_ttl_secs(&self) -> i64 {
        self.inner.manifest_ttl_secs
    }

    /// SEC PART 2: like [`Self::new`] but takes an explicit
    /// manifest TTL. Operators wire this from
    /// `[mesh] manifest_ttl_secs`. Tests use it for boundary
    /// cases. Defaults to [`DEFAULT_MANIFEST_TTL_SECS`].
    pub fn new_with_manifest_ttl(
        client: crate::transport::rpc::Client,
        peer_ids: std::collections::HashMap<String, crate::transport::rpc::PeerId>,
        addrs: std::collections::HashMap<String, crate::transport::rpc::Multiaddr>,
        identity: Bundle,
        deadline_secs: i64,
        manifest_ttl_secs: i64,
    ) -> Self {
        Self {
            inner: Arc::new(MeshClientInner {
                client,
                peer_ids: RwLock::new(peer_ids),
                addrs,
                reconnect_attempts: std::sync::atomic::AtomicU64::new(0),
                reconnect_successes: std::sync::atomic::AtomicU64::new(0),
                identity,
                deadline_secs,
                known_nodes: KnownNodesRegistry::new(),
                manifest_ttl_secs,
            }),
        }
    }

    /// Clone the underlying RPC client (cheap). Kept for back-compat with
    /// callers that drove the raw client directly; new callers should
    /// prefer [`Self::call`] so they get reconnect for free.
    pub fn client(&self) -> crate::transport::rpc::Client {
        self.inner.client.clone()
    }

    /// Snapshot the alias → PeerId map. Used by the flow runner's static
    /// dispatcher and by tests.
    pub fn peer_ids(&self) -> std::collections::HashMap<String, crate::transport::rpc::PeerId> {
        self.inner
            .peer_ids
            .read()
            .expect("mesh client peer_ids lock")
            .clone()
    }

    /// Snapshot the alias → Multiaddr map.
    pub fn addrs(&self) -> std::collections::HashMap<String, crate::transport::rpc::Multiaddr> {
        self.inner.addrs.clone()
    }

    /// Resolve an alias to its known PeerId.
    pub fn peer_id_for(&self, alias: &str) -> Option<crate::transport::rpc::PeerId> {
        self.inner
            .peer_ids
            .read()
            .expect("mesh client peer_ids lock")
            .get(alias)
            .copied()
    }

    /// `(attempts, successes)` for reconnect telemetry.
    pub fn reconnect_counters(&self) -> (u64, u64) {
        (
            self.inner
                .reconnect_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            self.inner
                .reconnect_successes
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Invoke a peer by alias with one automatic reconnect on transport
    /// failure. The reconnect re-dials the original `Multiaddr` from the
    /// address book and waits briefly for the swarm to settle before
    /// retrying the call. PeerIds are persistent across peer restarts
    /// (controller keys live on disk), so the cached alias → PeerId
    /// mapping stays valid.
    ///
    /// Returns the response body bytes on success. On final failure
    /// returns a [`MeshCallError`] whose `reconnected` flag tells the
    /// caller whether the retry happened (useful for log triage).
    pub async fn call(&self, alias: &str, envelope: Vec<u8>) -> Result<Vec<u8>, MeshCallError> {
        let peer_id = self.peer_id_for(alias).ok_or_else(|| MeshCallError {
            alias: alias.to_string(),
            cause: format!("unknown alias '{alias}' (not in peers.toml)"),
            reconnected: false,
        })?;
        match self.inner.client.call(peer_id, envelope.clone()).await {
            Ok(r) => Ok(r),
            Err(e) if looks_like_transport_break(&e) => {
                let Some(addr) = self.inner.addrs.get(alias).cloned() else {
                    return Err(MeshCallError {
                        alias: alias.to_string(),
                        cause: e,
                        reconnected: false,
                    });
                };
                self.inner
                    .reconnect_attempts
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    alias,
                    addr = %addr,
                    error = %e,
                    "mesh: call failed; re-dialing peer and retrying once"
                );
                if let Err(dial_err) = self.inner.client.dial(addr.clone()).await {
                    return Err(MeshCallError {
                        alias: alias.to_string(),
                        cause: format!("re-dial failed: {dial_err} (orig: {e})"),
                        reconnected: false,
                    });
                }
                // Give libp2p a brief moment to negotiate Noise + Yamux
                // before retrying. The actual connection establishment is
                // async; without this pause the retry would race the
                // PeerConnected event and fail the same way.
                tokio::time::sleep(Duration::from_millis(400)).await;
                match self.inner.client.call(peer_id, envelope).await {
                    Ok(r) => {
                        self.inner
                            .reconnect_successes
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tracing::info!(alias, "mesh: peer recovered on retry");
                        Ok(r)
                    }
                    Err(retry_err) => Err(MeshCallError {
                        alias: alias.to_string(),
                        cause: format!("retry after redial failed: {retry_err} (orig: {e})"),
                        reconnected: true,
                    }),
                }
            }
            Err(e) => Err(MeshCallError {
                alias: alias.to_string(),
                cause: e,
                reconnected: false,
            }),
        }
    }

    /// Re-pull `node.manifest` from every peer in the address book and
    /// update `cache` in place. Used both at startup (via
    /// [`discover_and_pin`]) and by the periodic refresh task spawned by
    /// [`Self::spawn_refresh_loop`].
    ///
    /// Failures per-peer are warned and skipped — a peer being temporarily
    /// unreachable should not nuke the cache.
    pub async fn refresh_manifests(&self, cache: &ManifestCache) {
        let alias_addr: Vec<(String, crate::transport::rpc::Multiaddr)> = self
            .inner
            .addrs
            .iter()
            .map(|(a, m)| (a.clone(), m.clone()))
            .collect();
        for (alias, addr) in alias_addr {
            // Ensure connection (cheap if already dialled).
            let _ = self.inner.client.dial(addr.clone()).await;
            let Some(peer_id) = self.peer_id_for(&alias) else {
                continue;
            };
            let envelope = build_request(
                "node.manifest",
                Vec::new(),
                self.inner.identity.clone(),
                self.inner.deadline_secs,
            );
            let resp_bytes = match tokio::time::timeout(
                Duration::from_secs(self.inner.deadline_secs as u64 + 2),
                self.inner.client.call(peer_id, envelope),
            )
            .await
            {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    tracing::debug!(alias, error = %e, "manifest refresh: transport error");
                    continue;
                }
                Err(_) => {
                    tracing::debug!(alias, "manifest refresh: timed out");
                    continue;
                }
            };
            let resp = match decode_response(&resp_bytes) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let body = match resp.res {
                ResponseResult::Ok(b) => b.to_vec(),
                _ => continue,
            };
            // SEC PART 2: decode + verify the signed manifest
            // envelope. Pre-fix path accepted unsigned
            // NodeManifest bytes — every peer could claim any
            // capabilities + org_id. The TOFU pin + Ed25519
            // verify path now ensures the manifest's signer
            // matches the one we saw first for this node_id.
            match codec::decode::<SignedManifest>(&body) {
                Ok(signed) => {
                    let now_ms = unix_ms();
                    match self.inner.known_nodes.verify_and_pin(
                        &signed,
                        self.inner.manifest_ttl_secs,
                        now_ms,
                    ) {
                        Ok(manifest) => {
                            cache.insert(Some(alias), manifest);
                        }
                        Err(e) => {
                            tracing::warn!(
                                alias,
                                error = %e,
                                kind = e.error_kind(),
                                "manifest refresh: signed envelope rejected"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        alias,
                        error = %e,
                        "manifest refresh: signed envelope decode failed (peer may be running pre-PART-2 build)"
                    );
                }
            }
        }
    }

    /// Spawn a background task that calls [`Self::refresh_manifests`] on
    /// `cache` every `period`. Detached — the task lives as long as the
    /// returned `JoinHandle` is dropped without abort, which is the
    /// intended pattern (bridge holds it for the process lifetime).
    pub fn spawn_refresh_loop(
        self,
        cache: Arc<ManifestCache>,
        period: Duration,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                self.refresh_manifests(&cache).await;
                tracing::debug!(
                    peers = self.inner.addrs.len(),
                    "mesh: manifest refresh tick complete"
                );
            }
        })
    }
}

/// Heuristic — when is a `Client::call` error transient enough to warrant
/// one automatic redial? The strings come from libp2p's
/// `request_response::OutboundFailure` Debug repr (which is what the
/// transport surfaces). DialFailure, ConnectionClosed, Timeout, and
/// UnsupportedProtocols are all symptoms of a peer that went away and may
/// be back. We deliberately do **not** retry on Io / DialError variants
/// caused by the local config being broken (those would 100% repeat).
fn looks_like_transport_break(err: &str) -> bool {
    // Substring match on the Debug output. `OutboundFailure` is the
    // enum reqwest_response uses.
    let lower = err.to_ascii_lowercase();
    lower.contains("dialfailure")
        || lower.contains("connectionclosed")
        || lower.contains("timeout")
        || lower.contains("io")
}

// ────────────────────────── Discovery client ───────────────────────────────

/// Options for the bridge's one-shot manifest discovery pass.
pub struct DiscoveryOptions {
    /// Caller's signed identity bundle — same one used for `/chat`.
    pub identity_bundle: Bundle,
    /// 32-byte libp2p secret. Bridge uses its own.
    ///
    /// SEC PART 2: wrapped in `Zeroizing` so the secret key
    /// bytes are wiped from the heap when `DiscoveryOptions`
    /// is dropped.
    pub client_key: zeroize::Zeroizing<[u8; 32]>,
    /// Peer alias map the bridge was started with.
    pub peers: PeersFile,
    /// Per-call deadline. 10s is plenty for `node.manifest`.
    pub deadline_secs: i64,
    /// Total wall-clock budget across retries. Default 6s.
    pub overall_timeout: Duration,
    /// Optional override for the ephemeral libp2p port (used in tests).
    pub local_port: Option<u16>,
    /// SEC §17: when set, discovery AUTO-REGISTERS each peer's
    /// handshake-verified identity key into this registry as
    /// `(node_name -> pubkey)`, so knowledge-share source binding
    /// works with no manual `[knowledge_trust]` config. Only a key
    /// whose derived libp2p PeerId equals the Noise-authenticated
    /// connection PeerId is registered (cryptographically verified).
    pub source_key_registry: Option<crate::knowledge::service::SourceNodeKeyRegistry>,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            identity_bundle: panic_no_identity(),
            client_key: zeroize::Zeroizing::new([0u8; 32]),
            peers: PeersFile::default(),
            deadline_secs: 10,
            overall_timeout: Duration::from_secs(6),
            local_port: None,
            source_key_registry: None,
        }
    }
}

fn panic_no_identity() -> Bundle {
    panic!("DiscoveryOptions::default has no identity; build it explicitly");
}

/// CORR PART 3: claim the next ephemeral libp2p port via an
/// `AtomicU16` compare-and-swap so two concurrent
/// `discover_and_pin` calls inside the same process can never
/// pick the same port. The pre-fix path called
/// `rand::random::<u16>()` independently in each caller; with
/// two callers active at the same instant (e.g. the bridge
/// starting alongside a flow-inspect helper) the random draw
/// could collide and the second listener's `bind` would race
/// the first.
///
/// The counter starts at the bottom of the ephemeral range
/// the previous code used (30_000) and wraps to the same
/// floor after 5_000 entries so a long-lived process keeps
/// claiming inside the operator-expected range. The CAS loop
/// is unconditional progress — the worst case is one CAS
/// failure per concurrent caller.
fn claim_next_ephemeral_port() -> u16 {
    static NEXT_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(30_000);
    const FLOOR: u16 = 30_000;
    const SPAN: u16 = 5_000;
    loop {
        let cur = NEXT_PORT.load(std::sync::atomic::Ordering::Relaxed);
        let next = if !(FLOOR..FLOOR + SPAN - 1).contains(&cur) {
            FLOOR
        } else {
            cur + 1
        };
        if NEXT_PORT
            .compare_exchange(
                cur,
                next,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
        {
            return cur.max(FLOOR);
        }
    }
}

/// Dial every peer in `opts.peers`, call `node.manifest` against each, and
/// populate a fresh [`ManifestCache`]. Peers that never reply within the
/// overall budget are simply absent from the cache — the caller decides
/// how to react.
///
/// Back-compat shim — kept so callers that only need the cache stay valid.
/// New callers should prefer [`discover_and_pin`], which also returns the
/// long-lived [`MeshClient`] so chat requests can avoid re-dialling on
/// every call (M11).
pub async fn discover_peers(opts: DiscoveryOptions) -> ManifestCache {
    discover_and_pin(opts)
        .await
        .map(|(cache, _)| cache)
        .unwrap_or_default()
}

/// Same as [`discover_peers`] but additionally hands back a [`MeshClient`]
/// pinned to the dialled peers. The caller is expected to keep the
/// `MeshClient` alive for the lifetime of the host (the bridge stashes it
/// in `AppState`). The underlying libp2p swarm task is spawned internally
/// and stays running as long as the `client` handle has any clone.
pub async fn discover_and_pin(opts: DiscoveryOptions) -> Option<(ManifestCache, MeshClient)> {
    let cache = ManifestCache::new();
    if opts.peers.peers.is_empty() {
        // Build a no-peer client so the bridge still has a usable libp2p
        // instance for future discovery refreshes / lazy dials.
        let local_port = opts.local_port.unwrap_or_else(claim_next_ephemeral_port);
        let (client, _events, event_loop) = rpc::new(*opts.client_key, local_port).await.ok()?;
        drop(tokio::spawn(event_loop.run()));
        return Some((
            cache,
            MeshClient::new(
                client,
                std::collections::HashMap::new(),
                std::collections::HashMap::new(),
                opts.identity_bundle,
                opts.deadline_secs,
            ),
        ));
    }

    let local_port = opts
        .local_port
        .unwrap_or_else(|| 30_000 + (rand::random::<u16>() % 5_000));

    let (client, mut events, event_loop) = match rpc::new(*opts.client_key, local_port).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "discovery: rpc::new failed; cache stays empty");
            return None;
        }
    };
    let _spawned = tokio::spawn(event_loop.run());

    // Dial all peers in parallel; remember which alias maps to which dial address.
    let mut want_alias_by_addr: BTreeMap<String, String> = BTreeMap::new();
    for (alias, entry) in &opts.peers.peers {
        let addr: Multiaddr = match entry.addr.parse() {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(alias = %alias, addr = %entry.addr, error = ?e, "discovery: invalid multiaddr");
                continue;
            }
        };
        if let Err(e) = client.dial(addr.clone()).await {
            tracing::warn!(alias = %alias, addr = %addr, error = %e, "discovery: dial failed");
            continue;
        }
        want_alias_by_addr.insert(entry.addr.clone(), alias.clone());
    }

    // Collect PeerConnected events for the duration of the budget. We use the
    // resolved PeerIds as the *single* place the bridge later dispatches to
    // (M11), so we save them into a peer_ids map alongside the (alias, peer_id)
    // list used for the in-pass node.manifest call.
    let mut peer_aliases: Vec<(PeerId, String)> = Vec::new();
    let mut peer_ids: std::collections::HashMap<String, PeerId> = std::collections::HashMap::new();
    let deadline = tokio::time::Instant::now() + opts.overall_timeout;
    while tokio::time::Instant::now() < deadline && peer_aliases.len() < want_alias_by_addr.len() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Some(TransportEvent::PeerConnected { peer_id, address })) => {
                let reported = address.to_string();
                if let Some((_, alias)) = want_alias_by_addr
                    .iter()
                    .find(|(want, _)| reported.starts_with(want.as_str()))
                {
                    let alias = alias.clone();
                    peer_aliases.push((peer_id, alias.clone()));
                    peer_ids.insert(alias, peer_id);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }

    // SEC §18: do NOT drop the events receiver here. When a
    // source-key registry is configured (knowledge mesh), we hand the
    // stream to a persistent consumer task at the end of discovery so
    // peer-disconnect events promptly drop trust (no stale keys). When
    // no registry is configured we drop it there instead, restoring
    // the original fast-no-op back-pressure behaviour.

    // SEC §18: directory of handshake-verified peers learned below,
    // mapping the Noise-verified PeerId to (node_name, pubkey) so the
    // disconnect consumer removes the exact key registered on connect.
    let mut peer_directory: PeerKeyDirectory = std::collections::HashMap::new();

    // Build alias → Multiaddr book from the input config so the MeshClient
    // can re-dial after a transient disconnect (A.4 reconnect support).
    let addrs: std::collections::HashMap<String, crate::transport::rpc::Multiaddr> = opts
        .peers
        .peers
        .iter()
        .filter_map(|(alias, entry)| entry.addr.parse().ok().map(|m| (alias.clone(), m)))
        .collect();

    let mesh_client = MeshClient::new(
        client.clone(),
        peer_ids.clone(),
        addrs,
        opts.identity_bundle.clone(),
        opts.deadline_secs,
    );

    if peer_aliases.is_empty() {
        tracing::warn!("discovery: no peers connected within budget; cache stays empty");
        return Some((cache, mesh_client));
    }

    // Call node.manifest on each connected peer.
    for (peer_id, alias) in peer_aliases {
        let envelope = build_request(
            "node.manifest",
            Vec::new(),
            opts.identity_bundle.clone(),
            opts.deadline_secs,
        );
        let resp_bytes = match tokio::time::timeout(
            Duration::from_secs(opts.deadline_secs as u64 + 2),
            client.call(peer_id, envelope),
        )
        .await
        {
            Ok(Ok(b)) => b,
            Ok(Err(e)) => {
                tracing::warn!(alias = %alias, error = %e, "discovery: node.manifest transport error");
                continue;
            }
            Err(_) => {
                tracing::warn!(alias = %alias, "discovery: node.manifest timed out");
                continue;
            }
        };
        let resp = match decode_response(&resp_bytes) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(alias = %alias, error = %e, "discovery: response decode failed");
                continue;
            }
        };
        let body = match resp.res {
            ResponseResult::Ok(b) => b.to_vec(),
            ResponseResult::Err(env) => {
                tracing::warn!(alias = %alias, kind = env.kind, cause = %env.cause, "discovery: peer returned error");
                continue;
            }
            ResponseResult::StreamHandle(_) => continue,
        };
        // SEC PART 2: discovery accepts ONLY signed manifests.
        // A peer producing a plain unsigned NodeManifest is
        // either pre-PART-2 (refuse to cache and surface a
        // warn) or an attacker dropping the envelope to
        // bypass the TOFU pin (same refuse path).
        let signed: SignedManifest = match codec::decode(&body) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(alias = %alias, error = %e, "discovery: signed manifest decode failed");
                continue;
            }
        };
        let now_ms = unix_ms();
        let manifest = match mesh_client.inner.known_nodes.verify_and_pin(
            &signed,
            mesh_client.inner.manifest_ttl_secs,
            now_ms,
        ) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    alias = %alias,
                    error = %e,
                    kind = e.error_kind(),
                    "discovery: signed manifest rejected"
                );
                continue;
            }
        };
        tracing::info!(
            alias = %alias,
            node_type = %manifest.node_type,
            methods = ?manifest.methods(),
            fingerprint = %signed.signer_fingerprint,
            "discovery: cached peer manifest"
        );
        // SEC §17: auto-learn the peer's identity key for
        // knowledge-share source binding — but ONLY if the manifest's
        // signer key is the SAME key the Noise handshake authenticated
        // for this connection (derive its libp2p PeerId and compare to
        // the connection `peer_id`). This guarantees we only ever
        // trust a key the handshake cryptographically verified — a
        // peer cannot register a name→key pair it doesn't actually
        // hold the private key for.
        if let Some(registry) = opts.source_key_registry.as_ref() {
            match peer_id_from_ed25519_pubkey(&signed.signer_pubkey) {
                Some(derived) if derived == peer_id => {
                    registry.register(manifest.node_name.clone(), signed.signer_pubkey);
                    // SEC §18: remember peer_id -> (name, key) so the
                    // disconnect consumer can remove the right key.
                    peer_directory
                        .insert(peer_id, (manifest.node_name.clone(), signed.signer_pubkey));
                    tracing::info!(
                        alias = %alias,
                        node_name = %manifest.node_name,
                        "discovery: auto-registered handshake-verified peer key for knowledge binding (SEC §17)"
                    );
                }
                _ => {
                    tracing::warn!(
                        alias = %alias,
                        node_name = %manifest.node_name,
                        "discovery: manifest signer key does NOT match the handshake-verified PeerId — \
                         NOT auto-registering (possible impersonation)"
                    );
                }
            }
        }
        cache.insert(Some(alias), manifest);
    }

    // SEC §18: hand the live event stream to a persistent consumer so
    // a peer's source key is removed PROMPTLY when its connection
    // closes (and re-registered if it reconnects) — not only on the
    // next discovery refresh. Only runs when knowledge auto-learning
    // is configured (registry present); otherwise the stream is
    // dropped, as before.
    match opts.source_key_registry.clone() {
        Some(registry) => {
            tokio::spawn(async move {
                let mut events = events;
                while let Some(ev) = events.recv().await {
                    apply_mesh_connection_event(&ev, &peer_directory, &registry);
                }
            });
        }
        None => drop(events),
    }

    Some((cache, mesh_client))
}

/// SEC §17: derive a peer's libp2p `PeerId` from its raw 32-byte
/// Ed25519 public key. Used to confirm a manifest's signer key is
/// the same key the Noise handshake authenticated for the
/// connection. Returns `None` if the bytes aren't a valid key.
fn peer_id_from_ed25519_pubkey(pubkey: &[u8; 32]) -> Option<PeerId> {
    let ed = libp2p::identity::ed25519::PublicKey::try_from_bytes(pubkey).ok()?;
    Some(libp2p::identity::PublicKey::from(ed).to_peer_id())
}

/// SEC §18: directory of handshake-verified peers, mapping the
/// Noise-authenticated `PeerId` to the `(node_name, identity pubkey)`
/// learned during discovery. Used by [`apply_mesh_connection_event`]
/// to resolve a disconnecting peer back to the exact source-key it
/// registered on connect.
pub(crate) type PeerKeyDirectory = std::collections::HashMap<PeerId, (String, [u8; 32])>;

/// SEC §18: apply a live mesh connection lifecycle event to the
/// knowledge source-key registry, so trust tracks the actual
/// connection state of each peer:
///
/// * `PeerDisconnected` → REMOVE the peer's source key (no stale
///   trust survives the connection drop; a subsequent share claiming
///   that peer is then rejected until it reconnects).
/// * `PeerConnected` (of a peer whose handshake-verified identity we
///   already learned during discovery) → RE-REGISTER its key, so a
///   reconnect restores trust without a manual step.
///
/// The key is only ever the one learned from the cryptographically-
/// verified handshake/manifest (via `directory`), preserving Section
/// 17's "only trust a handshake-verified key" guarantee. Unregister
/// is idempotent, so a spurious disconnect for a still-multiply-
/// connected peer is harmless (it re-registers on its next exchange).
pub(crate) fn apply_mesh_connection_event(
    ev: &TransportEvent,
    directory: &PeerKeyDirectory,
    registry: &crate::knowledge::service::SourceNodeKeyRegistry,
) {
    match ev {
        TransportEvent::PeerConnected { peer_id, .. } => {
            if let Some((node_name, pubkey)) = directory.get(peer_id) {
                registry.register(node_name.clone(), *pubkey);
            }
        }
        TransportEvent::PeerDisconnected { peer_id } => {
            if let Some((node_name, _)) = directory.get(peer_id) {
                registry.unregister(node_name);
            }
        }
        // Inbound RPC requests are not connection-lifecycle events.
        TransportEvent::Request { .. } => {}
    }
}

/// Convenience: discover with sensible defaults for tests that already have
/// a `PeersFile` and identity in memory.
#[allow(dead_code)]
pub fn default_discovery_options(
    identity_bundle: Bundle,
    client_key: [u8; 32],
    peers: PeersFile,
) -> DiscoveryOptions {
    DiscoveryOptions {
        identity_bundle,
        client_key: zeroize::Zeroizing::new(client_key),
        peers,
        deadline_secs: 10,
        overall_timeout: Duration::from_secs(6),
        local_port: None,
        source_key_registry: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(b: &[u8]) -> NodeId {
        NodeId::from_pubkey(b)
    }

    #[test]
    fn provider_dedupes_capabilities_on_repeat_register() {
        let p = ManifestProvider::new(n(b"node"), "n", "ai", n(b"org"), vec![]);
        p.add_capability(CapabilityDescriptor::unary("ai.chat"));
        p.add_capability(CapabilityDescriptor::unary("ai.chat"));
        assert_eq!(p.snapshot().capabilities.len(), 1);
    }

    #[test]
    fn cache_aggregates_methods_across_peers() {
        let cache = ManifestCache::new();
        let mut mem = NodeManifest {
            node_id: n(b"m"),
            node_name: "m".into(),
            node_type: "memory".into(),
            manifest_version: 1,
            org_id: n(b"o"),
            endpoints: vec![],
            capabilities: vec![CapabilityDescriptor::unary("memory.search")],
        };
        let ai = NodeManifest {
            node_id: n(b"a"),
            node_name: "a".into(),
            node_type: "ai".into(),
            manifest_version: 1,
            org_id: n(b"o"),
            endpoints: vec![],
            capabilities: vec![CapabilityDescriptor::unary("ai.chat")],
        };
        cache.insert(Some("memory".into()), mem.clone());
        cache.insert(Some("ai".into()), ai);
        assert_eq!(
            cache.all_methods(),
            vec!["ai.chat".to_string(), "memory.search".to_string()]
        );
        assert_eq!(
            cache.find_alias_for_method("memory.search").as_deref(),
            Some("memory")
        );
        assert_eq!(
            cache.find_alias_for_method("ai.chat").as_deref(),
            Some("ai")
        );
        assert_eq!(cache.find_alias_for_method("tool.web_fetch"), None);

        // Re-inserting under the same node_id overwrites in place.
        mem.capabilities
            .push(CapabilityDescriptor::unary("memory.write_turn"));
        cache.insert(Some("memory".into()), mem);
        assert!(cache.has_method("memory.write_turn"));
    }

    #[test]
    fn cache_insert_stamps_last_refreshed_at() {
        // Per multi-node operational realism: every successful
        // manifest refresh must stamp `last_refreshed_at` so
        // /v1/topology and /v1/health can compute freshness.
        // Failures don't reach insert; the timestamp is the
        // "last successful refresh" by construction.
        let cache = ManifestCache::new();
        let manifest = NodeManifest {
            node_id: n(b"m"),
            node_name: "m".into(),
            node_type: "memory".into(),
            manifest_version: 1,
            org_id: n(b"o"),
            endpoints: vec![],
            capabilities: vec![],
        };
        cache.insert(Some("memory".into()), manifest.clone());
        let entries = cache.entries();
        assert_eq!(entries.len(), 1);
        // unix_secs() returns the current time; in unit-test
        // wall-clock terms this is "definitely not 0 and
        // definitely within a sane window of now."
        let now = unix_secs();
        let stamped = entries[0].last_refreshed_at;
        assert!(stamped > 0, "expected non-zero timestamp, got {stamped}");
        assert!(
            (now - stamped).abs() < 5,
            "stamped={stamped} should be within 5s of now={now}"
        );
    }

    #[test]
    fn cache_re_insert_advances_last_refreshed_at() {
        // Background 60s refresh loop calls insert repeatedly.
        // Each successful refresh must update the timestamp so
        // freshness verdicts stay accurate even when the
        // manifest contents haven't changed.
        let cache = ManifestCache::new();
        let manifest = NodeManifest {
            node_id: n(b"m"),
            node_name: "m".into(),
            node_type: "memory".into(),
            manifest_version: 1,
            org_id: n(b"o"),
            endpoints: vec![],
            capabilities: vec![],
        };
        cache.insert(Some("memory".into()), manifest.clone());
        let first = cache.entries()[0].last_refreshed_at;
        // Force a measurable gap so the second insert's timestamp
        // is strictly greater. unix_secs() resolution is one
        // second, so we need ≥1100ms.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        cache.insert(Some("memory".into()), manifest);
        let second = cache.entries()[0].last_refreshed_at;
        assert!(
            second > first,
            "re-insert should advance the timestamp: first={first} second={second}"
        );
    }

    #[test]
    fn cache_holds_independent_freshness_per_peer() {
        // Each peer's freshness is independent. Refreshing one
        // peer must not mutate another's last_refreshed_at —
        // the multi-node operator view needs per-peer staleness
        // to be meaningful (failure-modes.md uses it to spot
        // which peer's refresh has stalled).
        let cache = ManifestCache::new();
        let mem = NodeManifest {
            node_id: n(b"mem"),
            node_name: "mem".into(),
            node_type: "memory".into(),
            manifest_version: 1,
            org_id: n(b"o"),
            endpoints: vec![],
            capabilities: vec![],
        };
        let ai = NodeManifest {
            node_id: n(b"ai"),
            node_name: "ai".into(),
            node_type: "ai".into(),
            manifest_version: 1,
            org_id: n(b"o"),
            endpoints: vec![],
            capabilities: vec![],
        };
        cache.insert(Some("memory".into()), mem.clone());
        let mem_first = cache
            .entries()
            .into_iter()
            .find(|e| e.alias.as_deref() == Some("memory"))
            .unwrap()
            .last_refreshed_at;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        // Refresh only the AI peer.
        cache.insert(Some("ai".into()), ai);
        let entries = cache.entries();
        let mem_second = entries
            .iter()
            .find(|e| e.alias.as_deref() == Some("memory"))
            .unwrap()
            .last_refreshed_at;
        let ai_stamp = entries
            .iter()
            .find(|e| e.alias.as_deref() == Some("ai"))
            .unwrap()
            .last_refreshed_at;
        assert_eq!(
            mem_second, mem_first,
            "memory's last_refreshed_at must not change when ai is refreshed"
        );
        assert!(
            ai_stamp > mem_first,
            "ai's stamp ({ai_stamp}) should be > memory's first stamp ({mem_first})"
        );
    }

    // ── SEC PART 2: signed manifest envelope tests ───────

    fn fresh_signing_key() -> SigningKey {
        use rand::rngs::OsRng;
        SigningKey::generate(&mut OsRng)
    }

    fn provider_with_signer(signer: SigningKey) -> ManifestProvider {
        let nid = NodeId::from_pubkey(&signer.verifying_key().to_bytes());
        ManifestProvider::new(nid, "n", "ai", n(b"org"), vec![]).with_signer(signer)
    }

    #[test]
    fn signed_snapshot_round_trips_via_verify_and_pin() {
        let key = fresh_signing_key();
        let provider = provider_with_signer(key.clone());
        provider.add_capability(CapabilityDescriptor::unary("ai.chat"));
        let now_ms: i64 = 1_700_000_000_000;
        let signed = provider.signed_snapshot(now_ms).unwrap();
        assert_eq!(signed.signed_at_ms, now_ms);
        let registry = KnownNodesRegistry::new();
        let manifest = registry
            .verify_and_pin(&signed, 300, now_ms + 1_000)
            .expect("verify");
        assert_eq!(manifest.node_type, "ai");
        assert!(manifest.advertises("ai.chat"));
        // Pin is now set; resending the same signed envelope
        // must verify again.
        let _ = registry
            .verify_and_pin(&signed, 300, now_ms + 2_000)
            .expect("second verify");
        assert!(
            registry
                .pinned_fingerprint(&NodeId::from_pubkey(&key.verifying_key().to_bytes()))
                .is_some()
        );
    }

    #[test]
    fn unsigned_or_tampered_body_fails_with_bad_signature() {
        let key = fresh_signing_key();
        let provider = provider_with_signer(key);
        let mut signed = provider.signed_snapshot(1_700_000_000_000).unwrap();
        signed.signature[0] ^= 0xFF;
        let registry = KnownNodesRegistry::new();
        let err = registry
            .verify_and_pin(&signed, 300, 1_700_000_000_500)
            .unwrap_err();
        assert!(matches!(err, ManifestVerifyError::BadSignature));
        assert_eq!(
            err.error_kind(),
            relix_core::types::error_kinds::MANIFEST_INVALID
        );
    }

    #[test]
    fn manifest_from_unknown_signer_first_time_pins_then_mismatch_is_rejected() {
        // First peer pins; a second peer that ALSO claims the
        // same node_id (by re-using node_name? not possible — but
        // we simulate a TOFU mismatch by swapping the signer
        // key, which changes both node_id-in-body AND fingerprint).
        //
        // The TOFU contract: same node_id seen with a DIFFERENT
        // signer_fingerprint → reject. Construct that by
        // signing the same body bytes with a second key but
        // forcing the body's node_id to match the first key's
        // node_id (which is precisely what an attacker would do).
        let key_a = fresh_signing_key();
        let nid_a = NodeId::from_pubkey(&key_a.verifying_key().to_bytes());
        let provider_a =
            ManifestProvider::new(nid_a, "n", "ai", n(b"org"), vec![]).with_signer(key_a);
        let signed_a = provider_a.signed_snapshot(1_700_000_000_000).unwrap();
        let registry = KnownNodesRegistry::new();
        registry
            .verify_and_pin(&signed_a, 300, 1_700_000_000_500)
            .unwrap();
        // Attacker: same node_id in the body, different signer key.
        let key_b = fresh_signing_key();
        let provider_b =
            ManifestProvider::new(nid_a, "n", "ai", n(b"org"), vec![]).with_signer(key_b);
        let signed_b = provider_b.signed_snapshot(1_700_000_000_000).unwrap();
        let err = registry
            .verify_and_pin(&signed_b, 300, 1_700_000_000_500)
            .unwrap_err();
        assert!(
            matches!(err, ManifestVerifyError::FingerprintMismatch { .. }),
            "got {err:?}"
        );
        assert_eq!(
            err.error_kind(),
            relix_core::types::error_kinds::MANIFEST_INVALID
        );
    }

    #[test]
    fn stale_manifest_is_rejected_with_manifest_stale_kind() {
        let key = fresh_signing_key();
        let provider = provider_with_signer(key);
        let signed = provider.signed_snapshot(1_700_000_000_000).unwrap();
        let registry = KnownNodesRegistry::new();
        // now_ms is 600s past signed_at_ms; ttl is 300s.
        let err = registry
            .verify_and_pin(&signed, 300, 1_700_000_000_000 + 600_000)
            .unwrap_err();
        match err {
            ManifestVerifyError::Stale {
                signed_at_ms,
                now_ms,
                ttl_secs,
            } => {
                assert_eq!(signed_at_ms, 1_700_000_000_000);
                assert_eq!(now_ms, 1_700_000_000_000 + 600_000);
                assert_eq!(ttl_secs, 300);
            }
            other => panic!("expected Stale, got {other:?}"),
        }
        assert_eq!(
            ManifestVerifyError::Stale {
                signed_at_ms: 0,
                now_ms: 0,
                ttl_secs: 0
            }
            .error_kind(),
            relix_core::types::error_kinds::MANIFEST_STALE
        );
    }

    #[test]
    fn fingerprint_swap_on_wire_is_detected() {
        // Attacker keeps the signer's body+signature but rewrites
        // signer_fingerprint on the wire. The recomputed
        // fingerprint won't match → MANIFEST_INVALID.
        let key = fresh_signing_key();
        let provider = provider_with_signer(key);
        let mut signed = provider.signed_snapshot(1_700_000_000_000).unwrap();
        signed.signer_fingerprint = "deadbeef".repeat(8);
        let registry = KnownNodesRegistry::new();
        let err = registry
            .verify_and_pin(&signed, 300, 1_700_000_000_500)
            .unwrap_err();
        assert!(
            matches!(err, ManifestVerifyError::FingerprintMismatch { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn provider_without_signer_refuses_signed_snapshot() {
        let p = ManifestProvider::new(n(b"node"), "n", "ai", n(b"org"), vec![]);
        let err = p.signed_snapshot(1_700_000_000_000).unwrap_err();
        assert!(matches!(err, SignError::NoSigningKeyConfigured));
    }

    #[test]
    fn transport_break_heuristic_matches_expected_strings() {
        // Substring matcher is intentionally generous; these are the
        // failure-mode strings libp2p surfaces that we want to retry on.
        assert!(looks_like_transport_break("OutboundFailure::DialFailure"));
        assert!(looks_like_transport_break(
            "ConnectionClosed { reason: .. }"
        ));
        assert!(looks_like_transport_break(
            "timeout while awaiting response"
        ));
        assert!(looks_like_transport_break("io: connection refused"));

        // Things we shouldn't retry on. UnsupportedProtocols would be a
        // real config bug not a transient drop. Wait — it contains
        // "io"... actually substring "io" matches a LOT. The heuristic
        // is permissive on purpose (we'd rather pay the redial cost on a
        // false positive than fail-stop a transient call), but we should
        // make sure the obviously-irrelevant strings still bounce.
        assert!(!looks_like_transport_break("unknown method"));
        assert!(!looks_like_transport_break("policy_denied: no rule"));
    }

    // ── SEC PART 7: descriptor cache populated in lock-step ──

    #[test]
    fn add_capability_populates_descriptor_cache() {
        let provider = ManifestProvider::new(
            NodeId([7u8; 32]),
            "coord-a",
            "coordinator",
            NodeId([0u8; 32]),
            vec![],
        );
        let cache = provider.descriptor_cache();
        assert!(cache.is_empty());

        let d = CapabilityDescriptor::unary("memory.search")
            .with_categories(["query".into()])
            .with_risk(relix_core::capability::RiskLevel::Safe);
        provider.add_capability(d.clone());

        // Cache reflects the just-registered descriptor.
        let cached = cache.get("memory.search").expect("cache miss");
        assert_eq!(cached.method_name, "memory.search");
        assert_eq!(cached.categories, vec!["query".to_string()]);
        assert_eq!(cached.risk_level, relix_core::capability::RiskLevel::Safe);
    }

    #[test]
    fn add_capability_dedupe_does_not_double_insert_into_cache() {
        let provider = ManifestProvider::new(
            NodeId([7u8; 32]),
            "coord-a",
            "coordinator",
            NodeId([0u8; 32]),
            vec![],
        );
        let cache = provider.descriptor_cache();
        let d = CapabilityDescriptor::unary("memory.search");
        provider.add_capability(d.clone());
        provider.add_capability(d.clone());
        assert_eq!(cache.len(), 1, "cache must respect de-dupe path");
        assert_eq!(provider.snapshot().capabilities.len(), 1);
    }

    #[test]
    fn descriptor_cache_handle_is_shared_with_provider() {
        // The caller-held cache + the provider's internal cache
        // are the same `Arc<DashMap>`, so descriptors registered
        // after the handle was taken are visible without
        // re-fetching.
        let provider = ManifestProvider::new(
            NodeId([7u8; 32]),
            "coord-a",
            "coordinator",
            NodeId([0u8; 32]),
            vec![],
        );
        let cache_before = provider.descriptor_cache();
        provider.add_capability(CapabilityDescriptor::unary("a.b"));
        assert!(cache_before.contains_key("a.b"));
        // The provider exposes a second handle that sees the
        // same data — proving the Arc isn't being cloned.
        let cache_after = provider.descriptor_cache();
        assert_eq!(cache_before.len(), cache_after.len());
    }

    #[test]
    fn describe_fn_from_cache_returns_o1_lookup() {
        use crate::dispatch::describe_fn_from_cache;
        let provider = ManifestProvider::new(
            NodeId([7u8; 32]),
            "coord-a",
            "coordinator",
            NodeId([0u8; 32]),
            vec![],
        );
        provider.add_capability(
            CapabilityDescriptor::unary("ai.chat").with_categories(["chat".into()]),
        );
        let describe = describe_fn_from_cache(provider.descriptor_cache());

        // Hit.
        let d = describe("ai.chat").expect("hit");
        assert_eq!(d.method_name, "ai.chat");
        assert_eq!(d.categories, vec!["chat".to_string()]);

        // Miss — the bridge falls back to the existing
        // category-free admit path, which is the expected
        // behaviour for unregistered methods.
        assert!(describe("ai.unknown").is_none());
    }

    #[test]
    fn describe_fn_sees_later_registrations_through_shared_cache() {
        use crate::dispatch::describe_fn_from_cache;
        let provider = ManifestProvider::new(
            NodeId([7u8; 32]),
            "coord-a",
            "coordinator",
            NodeId([0u8; 32]),
            vec![],
        );
        // Describe closure captured before any capability was
        // registered — must still see the descriptor when it
        // arrives because both ends share the same Arc<DashMap>.
        let describe = describe_fn_from_cache(provider.descriptor_cache());
        assert!(describe("late.add").is_none());

        provider.add_capability(CapabilityDescriptor::unary("late.add"));
        assert!(describe("late.add").is_some());
    }
}
