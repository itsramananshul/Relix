# Relix Threat Model — Alpha

This is the initial threat model. Updated per gate per `SECURITY.md`.

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
- **A8 — Side-channel attacks on the local secrets vault.** Out of scope. Standard OS-level file-permission discipline; HSM at Gate 3.
- **A9 — Supply-chain attack on dependencies.** Baseline `cargo audit` only. Full supply-chain hardening at Gate 3.

## Assets

| Asset | Where | Who Owns It | Compromise Impact |
|---|---|---|---|
| Anthropic API key | AI node local config | AI node operator | Cost / quota burn; conversation interception |
| Org root keypair | `dev-keys/org-root.key` (alpha) | Org admin | Total mesh compromise (alpha) |
| Node identity keys | per-node local data dir | per-node operator | Impersonation of that node |
| User session JWT | Relix Web | per-user | Browser session takeover |
| Conversation history | Memory node SQLite | memory-node operator | Privacy breach |
| Audit logs | Per responder local | per-node operator | Audit trail blinding (detectable via gaps) |

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
- Policy evaluated on every responder.
- Audit emitted on every responder for every cross-node call.
- AI provider keys present ONLY in the AI node.
- Web backend makes no LLM provider call in `RELIX_MODE`.
- Routing decisions live only in SOL flows.

## Known Limitations (Tracked)

- SIMP-002: single-key trust model.
- SIMP-003: no CRL gossip.
- SIMP-004: allowlist policy instead of Cedar.
- SIMP-005: no event log snapshots.
- SIMP-008: no replay-equivalence property test.
- SIMP-012: no fuzz coverage in CI.

All of the above are deferred per `specs/alpha-simplifications.md` to Gate 2.
