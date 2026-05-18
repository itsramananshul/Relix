# RELIX-4 — Signed-Bundle Format

**Status:** Frozen target. Alpha implements simplified envelope (SIMP-002 single-key trust, no CRL).

## 4.1 Responsibilities

Shared cryptographic envelope for: credentials (AIC, GMC), federation certs, capability manifests, policy bundles, CRLs, emergency revocations, node manifests. One format, one verification path, three consumers (identity, policy, capability).

## 4.2 Invariants

1. Every bundle verifiable in isolation given a trust root.
2. Every bundle binds a payload to an issuer at a moment in time.
3. Every bundle has explicit `not_before` / `not_after`.
4. Every bundle is content-addressed by stable hash (`bundle_id` = BLAKE3-256 of encoded envelope).
5. Every bundle's revocation status queryable independent of content.

## 4.3 Encoding

COSE_Sign1 (RFC 8152) with fixed choices: `alg = -8` (Ed25519), deterministic CBOR per RFC 8949 §4.2.

## 4.4 Protected Headers (signed)

- `alg` (int, -8 for Ed25519)
- `kid` (bstr(32), issuer key ID = BLAKE3-256 of pubkey)
- `bundle_type` (tstr: `aic`, `gmc`, `capability_manifest`, `policy_bundle`, `crl`, `revoke_now`, `federation_cert`, `node_manifest`, `attenuated_token`)
- `bundle_format_version` (u8)

## 4.6 Payload Common Fields

- `issuer_id` (must equal `kid`)
- `subject_id` (conditional)
- `bundle_serial` (16 random bytes; uniquely identifies issuance)
- `not_before`, `not_after`
- `delegation_chain` (omitted iff issuer is trust root)

## 4.7 Trust Chain Validation

1. Decode COSE_Sign1; extract `kid`.
2. Resolve issuer pubkey via known trust root or walk delegation chain.
3. Each chain link verified, and parent must authorize child's `bundle_type`.
4. Verify Ed25519 signature.
5. Check `not_before ≤ now ≤ not_after` (±30 s skew).
6. Check revocation (CRL + revoke_now).
7. Check `bundle_format_version` supported.
8. Bundle-type-specific validation.

Failure errors distinguish: `signature_invalid`, `expired`, `revoked`, `untrusted_chain`, `format_unsupported`.

## 4.11 Expiration Defaults

- Trust roots: years
- IA bundles: months
- Credentials (AIC/GMC): hours-to-days
- Policy bundles: months (with rapid update cadence)
- Manifests: 7 days default
- CRLs: 1 hour

## 4.13 Revocation

Two mechanisms:
- **CRL** (`bundle_type: crl`): signed by issuer, lists `revoked_serials[]`. Gossiped.
- **`revoke_now`**: signed by Org Root for emergency. Pushed on high-priority gossip.

A bundle is valid iff serial not in current CRL AND `bundle_id` not in current `revoke_now`.

---

## Alpha Implementation Notes

Alpha ships a simplified bundle envelope (`relix-core::bundle`):
- COSE_Sign1-style: header (alg, kid, bundle_type, format_version), payload, Ed25519 sig — but implemented as a hand-rolled CBOR map for the alpha rather than full COSE_Sign1. Migration to full COSE_Sign1 at Gate 2.
- Bundle types implemented: `identity` (collapses AIC + GMC for alpha), `node_manifest`, `policy_bundle`.
- Delegation chain length = 0 (SIMP-002 single-key trust).
- No CRL gossip (SIMP-003); revocation by expiry only.
- Deterministic CBOR per `relix-core::codec` (ciborium with map-key canonicalization).
