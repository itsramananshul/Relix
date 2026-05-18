# Security

## Reporting Vulnerabilities

For security-relevant issues, send mail to `ramanal@mail.uc.edu` with `[RELIX SECURITY]` in the subject. Do not open a public GitHub issue for unfixed vulnerabilities.

Expected response:

- Acknowledgement within 48 hours.
- Triage within 5 business days.
- Patch and coordinated disclosure per the agreed timeline (default 90 days, faster for critical).

## Threat Model (Alpha)

The alpha threat model assumes the following attacker classes:

- **External unauthenticated network peer.** Cannot reach Relix nodes without an `IdentityBundle` validated by the local trust root. Per-RPC nonce + deadline + replay cache bounds replay.
- **Compromised identity holder within the org.** Bounded by group membership and per-method policy on the responder. Audit captures all activity.
- **Compromised single controller node.** Loses its own keys and any cached state. Does not expose any other node's keys (each node holds only its own).
- **Compromised IA / org-root key.** Catastrophic for the alpha (single-key model). Documented as alpha simplification; HSM and IA hierarchy at Gate 2.

Out of scope for the alpha:

- Federation across organizations.
- Side-channel attacks on the local secrets vault.
- Supply-chain attacks on dependencies beyond `cargo audit` baseline coverage.

## Secret Handling

- **No secrets in the repository.** API keys, private keys, and credentials are never committed. `.gitignore` excludes `*.key`, `*.pem`, `.env*`, `secrets/`.
- **AI provider keys** (Anthropic) live ONLY in the AI node's local config file. Verified at release time by `grep -ri ANTHROPIC relix-web/ crates/`.
- **Node identity keys** are generated on first start and stored in the controller's local data directory (default `~/.relix/<node-name>/identity.key`) with restricted file permissions.
- **Test keys** are generated per test run and never reused across runs.

## Key Rotation

- **Org-root key**: rotated manually for the alpha (single-key model). Documented runbook in `ops/runbooks/key-rotation.md` (post-alpha).
- **IA keys**: same as org-root for the alpha (collapsed into one).
- **Node identity keys**: regenerated on `relix-cli identity rotate-node <name>`. Manifests are re-signed and re-published.

## Cryptographic Choices

- Signatures: **Ed25519** (libp2p uses it natively for `PeerId`; we use it everywhere).
- Hashing: **BLAKE3-256** for bundle IDs and event-log chain.
- CBOR canonical encoding per RFC 8949 §4.2 for all signed payloads.
- Transport encryption: **Noise XK** via libp2p (inherits from OpenPrem `network/rpc.rs`).

## Known Limitations (Alpha)

These are documented because they are real, not because they are acceptable for production. Tracked in `specs/alpha-simplifications.md`:

- Single org-root key; no IA hierarchy.
- No CRL gossip; revocation by manifest expiry only.
- Synchronous SOL execution; long-running flows block VM threads.
- No fuzz coverage in CI yet.
- No formal security audit.

## Vulnerability Disclosure Hall of Fame

Will be published at Gate 3 (enterprise pilot readiness). Until then, private acknowledgement only.
