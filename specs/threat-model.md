# Relix Threat Model — Alpha

Version: 0.4.1

This is the threat model for the current alpha. Updated per security gate per
`SECURITY.md`.

## Attacker Classes Covered

### A1 — External Unauthenticated Network Peer

**Capabilities:** can attempt TCP connection to a Relix node. Has no valid identity credential.

**Mitigations:**
- libp2p Noise XK handshake required at connection level.
- No method dispatch occurs without a valid `IdentityBundle` (admission step 2).
- Rate limiting at the connection level (future).

**Residual risk:** connection-establishment DoS. Acceptable for alpha (mesh size ≤ 5 nodes).

### A2 — Compromised Identity Holder Within the Org

**Capabilities:** holds a valid `IdentityBundle`; can sign requests as their identity.

**Mitigations:**
- Each request is policy-evaluated on the responder. Compromise's blast radius = the union of capabilities allowed by their groups.
- Audit captures all activity. Compromise is detectable post-hoc.
- Short credential lifetimes (alpha: 24h) bound exposure window.

**Residual risk:** between compromise and detection, the attacker can do anything the compromised identity could do. Mitigation is detection latency + revocation latency, both documented in `SECURITY.md`.

### A3 — Compromised GMC Holder

**Capabilities:** holds a valid GMC granting an additional group.

**Mitigations:** same as A2 but scoped to actions requiring the specific group. Revocation by removing the GMC.

### A4 — Compromised Single Controller Node

**Capabilities:** loses its own private key and any cached state; can impersonate the node to peers.

**Mitigations:**
- Each node holds only its own identity key. Compromise does NOT expose other nodes' keys.
- API keys for external services (Anthropic) live only in the AI node; only its compromise exposes the LLM key.
- Audit on other nodes captures all calls the compromised node made; forensics is tractable.

**Residual risk:** the compromised node can sign arbitrary RPCs as itself. Limited by its own policy and group memberships.

### A5 — Compromised Org-Root Key (Alpha-Specific)

**Capabilities:** can sign arbitrary identities, manifests, policies as the org.

**Mitigations (alpha):** Org-root key kept offline; only `relix-cli identity` uses it; no daemon holds it. Documented in `SECURITY.md`.

**Residual risk (alpha):** if the org-root key file is compromised, the alpha mesh is compromised. The full mitigation (HSM, IA hierarchy, ceremony) lands at Gate 2 (SIMP-002).

## Attacker Classes NOT Covered in Alpha

- **A6 — Insider with admin role.** Out of scope. Org-internal trust assumed for alpha. Threat-model expansion at Gate 3.
- **A7 — Cross-org federation partner.** Out of scope. Federation not implemented in alpha.
- **A8 — Side-channel attacks on the local secrets vault.** Partially mitigated: the credential vault uses Argon2id KDF + AES-256-GCM at rest with `Zeroizing<>` in-memory hygiene. Full HSM-backed protection deferred to Gate 3.
- **A9 — Supply-chain attack on dependencies.** Baseline `cargo audit` only. The plugin subsystem adds optional per-plugin controls (`publisher_key` Ed25519 manifest signature, `binary_sha256` binary hash pin). Full supply-chain hardening at Gate 3.

## Assets

| Asset | Where | Who Owns It | Compromise Impact |
|---|---|---|---|
| Anthropic API key | AI node local config | AI node operator | Cost / quota burn; conversation interception |
| Org root keypair | `dev-keys/org-root.key` (alpha) | Org admin | Total mesh compromise (alpha) |
| Node identity keys | per-node local data dir | per-node operator | Impersonation of that node |
| User session JWT | Relix Web | per-user | Browser session takeover |
| Conversation history | Memory node SQLite | memory-node operator | Privacy breach |
| Audit logs | Per responder local | per-node operator | Audit trail blinding (detectable via gaps) |
| Credential vault | Coordinator SQLite (`credentials.db`) | coordinator operator | Exposure of all stored API keys and tokens |
| Approval signing key (`RELIX_APPROVAL_SIGNING_KEY`) | Coordinator process env | coordinator operator | Ability to forge approval tokens for any method |

## Attack Surface Per Node

### Memory node
- Inbound capabilities: `memory.search`, `memory.write_turn`, `memory.recent_for_session`.
- Holds: SQLite file with conversation history.
- Exposes: policy-gated read/write to its database.

### AI node
- Inbound capabilities: `ai.chat`.
- Holds: Anthropic API key.
- Exposes: policy-gated LLM access (consumes paid API budget).

### Tool node
- Inbound capabilities: `tool.web_fetch`.
- Holds: URL allowlist.
- Exposes: HTTP fetches to allowlisted URLs.

### Web bridge node
- Inbound capabilities: SSE endpoint over local HTTP (loopback only).
- Holds: nothing sensitive.
- Exposes: chat-flow trigger via local HTTP.

### Relix Web (presentation peer)
- Inbound: HTTPS from browser.
- Holds: user accounts, session JWTs, chat history (display copy).
- Does NOT hold: any LLM provider key, any Relix mesh credential.

## Existential Properties

If any of the following are violated, the alpha is compromised regardless of test results:

- Identity verified on every responder before any handler logic runs.
- Agent gate evaluated (when configured) before the policy engine; gate is
  fail-closed for missing store and missing profile.
- Policy evaluated on every responder; tenant isolation deny fires before
  per-method rules when enabled.
- Audit emitted on every responder for every cross-node call.
- AI provider keys present ONLY in the AI node.
- Web backend makes no LLM provider call in `RELIX_MODE`.
- Routing decisions live only in SOL flows.
- Credential vault master secret never written to disk; derived keys and
  plaintext values held only in `Zeroizing<>` heap memory.
- Approval token signing key (`RELIX_APPROVAL_SIGNING_KEY`) held only in
  coordinator process memory; never transmitted over the mesh.

## Shipped security controls not in original model

The following controls were added after the initial threat model was written
and are now enforced in code:

- **Fail-closed agent gate** — missing agent store or missing profile is a
  deny, not a no-op. The legacy silent-allow behaviour was removed.
- **Ed25519 approval tokens** — replaces the deprecated HMAC-SHA256 scheme.
  v0x01 tokens are refused at parse. See
  [`approval-tokens.md`](../docs/approval-tokens.md).
- **Encrypted credential vault** — AES-256-GCM + Argon2id at rest. Legacy
  SHA-256 vaults refused at open. See
  [`credentials.md`](../docs/credentials.md).
- **Plugin sandbox** — Linux seccomp (23-syscall deny list) + rlimits;
  TLS loopback transport with per-plugin cert pinning; binary SHA-256 and
  publisher-key signature verification.
- **Tenant isolation** — `TenantPolicyResolver` fail-closed deny when
  `tenant_id` is absent in multi-tenant mode.
- **Surface check fix** — agent gate reads transport-layer `caller_surface`
  (trusted); `envelope.surface` is ignored for admission.
- **Mesh PII gate** — optional inbound args scanning with block/redact/log
  actions and `pii_events.sqlite` audit trail.
- **Secret redaction** — 13 secret-kind patterns applied to log output and
  error messages via `redact_secrets`.

## Known Limitations (Tracked)

- SIMP-002: single-key trust model.
- SIMP-003: no CRL gossip.
- SIMP-004: allowlist policy instead of Cedar.
- SIMP-005: no event log snapshots.
- SIMP-008: no replay-equivalence property test.
- SIMP-012: no fuzz coverage in CI.

All of the above are deferred per `specs/alpha-simplifications.md` to Gate 2.
