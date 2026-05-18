# RELIX-5 — Node Manifest Format

**Status:** Frozen target. Alpha implements minimal manifest (SIMP-007 — no gossip).

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

On change to capabilities, endpoints, or runtime — or at 50% of lifetime. Increment `manifest_version`, re-sign, publish digest via gossip.

## 5.10 Stale Manifest

A peer holding an expired manifest treats the node as unreachable and refreshes; failure ⇒ `manifest_stale` error.

---

## Alpha Implementation Notes

Alpha ships:
- Manifest fields: `node_id`, `node_name`, `node_type`, `manifest_version`, `org_id`, `endpoints`, `capability_advertisement` (inline), `policy_bindings.node_policy_path` (filesystem path for alpha, not bundle id), `runtime.version`.
- No IA cosignature (SIMP-002); single-signed by node key.
- No gossip (SIMP-007); manifest exchanged on connect via `node.manifest` RPC.
- 7-day default `not_after`.
- `node_type` enum covers alpha set: `memory`, `ai`, `tool`, `web_bridge`, `presentation`, `dev_cli`.
