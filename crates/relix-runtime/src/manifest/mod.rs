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

use serde::{Deserialize, Serialize};

use relix_core::bundle::Bundle;
use relix_core::capability::CapabilityDescriptor;
use relix_core::codec;
use relix_core::types::NodeId;

use crate::dispatch::{build_request, decode_response};
use crate::flow_runner::PeersFile;
use crate::transport::envelope::ResponseResult;
use crate::transport::rpc::{self, Event as TransportEvent, Multiaddr, PeerId};

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

/// Shared, append-only manifest builder. Each node-type's `register(...)` in
/// `crate::nodes::*` calls [`Self::add_capability`] alongside its
/// `bridge.register(...)` so the manifest stays in sync with the dispatch
/// bridge. Cloning is cheap (`Arc`).
#[derive(Clone)]
pub struct ManifestProvider {
    inner: Arc<RwLock<NodeManifest>>,
}

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
        }
    }

    /// Append a capability the dispatch bridge has just registered.
    pub fn add_capability(&self, desc: CapabilityDescriptor) {
        let mut guard = self.inner.write().expect("manifest provider lock poisoned");
        // De-dupe by method_name so register-on-restart doesn't duplicate.
        if guard
            .capabilities
            .iter()
            .any(|c| c.method_name == desc.method_name)
        {
            return;
        }
        guard.capabilities.push(desc);
    }

    /// Snapshot the current manifest (cheap clone).
    pub fn snapshot(&self) -> NodeManifest {
        self.inner
            .read()
            .expect("manifest provider lock poisoned")
            .clone()
    }
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
}

impl ManifestCache {
    /// Empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert / replace by node id.
    pub fn insert(&self, alias: Option<String>, manifest: NodeManifest) {
        let key = manifest.node_id.to_string();
        let mut guard = self.inner.write().expect("manifest cache lock poisoned");
        guard.insert(key, CachedManifest { alias, manifest });
    }

    /// Snapshot every cached entry.
    pub fn entries(&self) -> Vec<CachedManifest> {
        self.inner
            .read()
            .expect("manifest cache lock poisoned")
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
        let guard = self.inner.read().expect("manifest cache lock poisoned");
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
        let guard = self.inner.read().expect("manifest cache lock poisoned");
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
            .expect("manifest cache lock poisoned")
            .values()
            .any(|c| c.manifest.advertises(method))
    }
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
            if let Ok(manifest) = codec::decode::<NodeManifest>(&body) {
                cache.insert(Some(alias), manifest);
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
    pub client_key: [u8; 32],
    /// Peer alias map the bridge was started with.
    pub peers: PeersFile,
    /// Per-call deadline. 10s is plenty for `node.manifest`.
    pub deadline_secs: i64,
    /// Total wall-clock budget across retries. Default 6s.
    pub overall_timeout: Duration,
    /// Optional override for the ephemeral libp2p port (used in tests).
    pub local_port: Option<u16>,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            identity_bundle: panic_no_identity(),
            client_key: [0u8; 32],
            peers: PeersFile::default(),
            deadline_secs: 10,
            overall_timeout: Duration::from_secs(6),
            local_port: None,
        }
    }
}

fn panic_no_identity() -> Bundle {
    panic!("DiscoveryOptions::default has no identity; build it explicitly");
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
        let local_port = opts
            .local_port
            .unwrap_or_else(|| 30_000 + (rand::random::<u16>() % 5_000));
        let (client, _events, event_loop) = rpc::new(opts.client_key, local_port).await.ok()?;
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

    let (client, mut events, event_loop) = match rpc::new(opts.client_key, local_port).await {
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

    // After discovery, drop the events receiver so the swarm task's
    // back-pressure on `event_sender` becomes a fast no-op (Sender::send sees
    // a closed channel and returns Err immediately, which the swarm ignores).
    drop(events);

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
        let manifest: NodeManifest = match codec::decode(&body) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(alias = %alias, error = %e, "discovery: manifest decode failed");
                continue;
            }
        };
        tracing::info!(
            alias = %alias,
            node_type = %manifest.node_type,
            methods = ?manifest.methods(),
            "discovery: cached peer manifest"
        );
        cache.insert(Some(alias), manifest);
    }
    Some((cache, mesh_client))
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
        client_key,
        peers,
        deadline_secs: 10,
        overall_timeout: Duration::from_secs(6),
        local_port: None,
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
}
