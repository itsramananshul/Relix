# RELIX-5 — Node Manifest Format

**Version:** 0.4.1 | **Status:** Frozen target. Alpha implements minimal manifest (SIMP-007 — no gossip).

## 5.1 Responsibilities

A signed-bundle (`bundle_type: node_manifest`) declaring a controller's identity, type, runtime, endpoints, advertised capabilities, policy bindings, version compatibility. Peers consult it to know what a node is and what it offers.

## 5.2 Invariants

1. Each controller has exactly one current manifest at any moment.
2. The manifest binds to the controller's peer ID via signature.
3. Manifests dual-signed in production: node key + IA cosignature.
4. Manifest expiration MUST be respected.
5. Capability claims in the manifest are authoritative.

## 5.3 Payload Fields (in addition to RELIX-4 common)

- `node_id` (peer ID; equals `subject_id`)
- `node_name` (human-readable within org)
- `node_type` (`ai` / `memory` / `channel` / `tool` / `bridge` / `presentation` / `audit` / `admin` / `issuer` / `policy_authority` / `capability_registry` / `custom:<name>`)
- `manifest_version` (u64; monotonic per node)
- `org_id` (org root key ID)
- `runtime` (relix runtime version, supported protocols, CDDL stdlib version, build id)
- `endpoints` (libp2p multiaddrs)
- `capability_advertisement` (inline OR digest+reference)
- `policy_bindings` (active policy bundle id, identity trust roots, policy trust roots, max staleness)
- `version_compatibility` (min peer relix version, per-protocol minimums)
- `node_co_signature` (conditional; IA cosig in production)

## 5.7 Validation

1. Validate as signed bundle (RELIX-4).
2. Verify `subject_id == node_id == signer`.
3. Production: verify `node_co_signature` against trusted IA.
4. Verify `org_id` per federation policy.
5. Verify peer's min relix version compat.
6. If capability advertisement is by reference: fetch + validate.

## 5.8 Startup

1. Load/generate identity keypair.
2. Construct manifest from config + registered capabilities.
3. Sign with own key.
4. Production: request IA cosignature.
5. Bind libp2p endpoints.
6. Load policy bundles.
7. Accept connections; serve `node.manifest`.

A controller MUST NOT serve any capability before steps 1–6.

## 5.9 Refresh

On change to capabilities, endpoints, or runtime — increment `manifest_version`, re-sign, publish digest via gossip. The "50% of lifetime" trigger does not apply in alpha (see Alpha Implementation Notes). Refresh period in alpha is operator-configured via the caller of `spawn_refresh_loop`.

## 5.10 Stale Manifest

A peer holding an expired manifest treats the node as unreachable and refreshes; failure ⇒ `manifest_stale` error.

---

## Alpha Implementation Notes (v0.4.1)

Alpha ships:

- **Signing mechanism:** `ManifestProvider::signed_snapshot(now_ms)` produces a `SignedManifest` envelope: `CBOR(NodeManifest)` signed with the node's Ed25519 key (`ed25519_dalek`). This is **not** the RELIX-4 `BundleType::NodeManifest` bundle chain described in §5.1; the full bundle chain is Gate 2 work.

  `SignedManifest` wire fields:
  - `body: Vec<u8>` — CBOR-encoded `NodeManifest`
  - `signature: [u8; 64]` — Ed25519 signature over `body` bytes
  - `signer_fingerprint: String` — `hex(blake3(signer_pubkey_bytes))`
  - `signer_pubkey: [u8; 32]` — raw Ed25519 public key (no prior out-of-band distribution required)
  - `signed_at_ms: i64` — millisecond timestamp of signing

- **Receiver verification order:** (1) decode envelope; (2) `blake3(signer_pubkey_bytes)` must equal `signer_fingerprint`; (3) Ed25519 signature over body bytes must verify; (4) TOFU pin check (see below); (5) freshness check `(now_ms - signed_at_ms) <= DEFAULT_MANIFEST_TTL_SECS * 1000`.

- **TOFU pinning:** first observed `signer_fingerprint` for a given `node_id` is pinned in `KnownNodesRegistry`. Subsequent manifests from the same node must present the same fingerprint. Pins are **in-memory only** — lost on process restart. A restarting receiver re-pins from the first inbound manifest.

- **Freshness window:** `DEFAULT_MANIFEST_TTL_SECS = 300` (5 minutes). This is applied by the **receiver** on each manifest exchange. Manifests have **no embedded expiry field** (`not_after`); the 5-minute freshness window is the sole staleness mechanism in alpha.

- **Refresh:** caller-configured period passed to `MeshClient::spawn_refresh_loop`. The bridge binary wires 60 s (A.4). There is no "refresh at 50% of lifetime" logic in alpha.

- **Manifest fields:** `node_id`, `node_name`, `node_type` (free-form string; not an enum), `manifest_version` (always 1 at boot in alpha), `org_id`, `endpoints`, `capabilities` (inline `Vec<CapabilityDescriptor>`).

- No IA cosignature (SIMP-002); single-signed by node key.
- No gossip (SIMP-007); manifest exchanged on connect via `node.manifest` RPC.
- `manifest_version` is always `1` today (static per binary launch); event-sourced version increment is Gate 2 work.
