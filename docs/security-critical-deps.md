# Security-Critical Dependencies

This document lists every third-party dependency whose compromise, misbehavior, or insecure update would have direct security impact on Relix. Bumps to these require explicit security review per `CONTRIBUTING.md`.

| Crate | Version | Purpose | Why Selected | Alternatives Considered |
|---|---|---|---|---|
| `libp2p` | 0.54 | P2P transport (TCP + Noise XK + Yamux), Kademlia DHT, CBOR `request_response`. | Inherited from OpenPrem `network/rpc.rs`; production-grade, audited by the libp2p ecosystem. Same version pin avoids integration churn. | None — libp2p is the de-facto standard for this use; rolling our own transport is out of scope. |
| `ed25519-dalek` | 2.x | Ed25519 signature primitive for identities, bundles, event records. | Pure Rust, constant-time, widely audited, RustCrypto org. | `ring` (heavier deps, FFI to BoringSSL); reject for pure-Rust preference. |
| `blake3` | 1.x | BLAKE3-256 for bundle IDs and event-log hash chain. | Fast, well-audited, pure-Rust. | SHA-256 — slower; bundle IDs do not need NIST compliance. |
| `ciborium` | 0.2 | CBOR encoder/decoder substrate. | Maintained, RFC 8949 conformant. We apply our own canonicalization layer on top per RFC 8949 §4.2. | `serde_cbor` (unmaintained); `minicbor` (less serde-friendly). |
| `rusqlite` | 0.34 (with `bundled` + `fulltext-search` features) | Memory node SQLite + FTS5 backend. Hermes-inspired schema. | Bundled libsqlite avoids version-skew with system SQLite. FTS5 is required for session search. | Direct `libsqlite3-sys` — too low-level. |
| `tokio` | 1.x | Async runtime. | Already the OpenPrem choice; required by libp2p tokio integration. | None pragmatic. |
| `serde` | 1.x | Serialization framework. | Used pervasively in OpenPrem and ecosystem. Carries no own security risk; risk is in serializers. | None. |
| `thiserror` | 2.x | Typed error derives. | Standard; zero runtime cost. | `anyhow` for binary crates only; `thiserror` mandatory for library error types per CONTRIBUTING.md. |
| `toml` | 0.8 | Config parsing. | Existing OpenPrem choice; small attack surface. | YAML — rejected (parser CVE history); JSON — fine but TOML matches existing config style. |
| `reqwest` | 0.12 (with `rustls-tls` only — no native-tls) | HTTP client for AI node (Anthropic) and tool node (web.fetch). | TLS via rustls (pure Rust, no system OpenSSL dep). | `hyper` directly — more work; `ureq` — synchronous-only. |
| `axum` | 0.7 | Web bridge HTTP server. | Lightweight, tower-based, async, mature. | `warp` — heavier; `hyper` raw — too low-level. |
| `tracing` | 0.1 | Structured logging. | Standard ecosystem choice. | `log` + `env_logger` — less structured. |
| `chrono` | 0.4 (or `time` 0.3) | Timestamps in audit + bundle expiry. | Decision pending Day 1 (`time` likely; smaller surface). | `chrono` (more features), `time` (smaller). |

## Dependency Policy

- New cryptographic or transport dependencies: explicit security review.
- All third-party dependencies pinned at workspace level (`Cargo.toml [workspace.dependencies]`).
- `cargo audit` in CI; security advisories in this list's crates block merges.
- `cargo deny` license whitelist: MIT, Apache-2.0, BSD-3-Clause, Unicode-DFS-2016, ISC.
- Forks / `[patch.crates-io]` require an open upstream issue and a documented timeline to remove.

## Reviewed Dates

| Crate | Last Reviewed | Reviewer |
|---|---|---|
| (initial entries) | 2026-05-18 | (alpha kickoff) |

## License Exceptions

### CDLA-Permissive-2.0 (`webpki-roots`)

**How it enters the tree:**

```
relix-runtime → reqwest 0.12 → hyper-rustls 0.27 → webpki-roots 1.0
relix-runtime → reqwest 0.12 → webpki-roots 1.0
```

`reqwest` is enabled with the `rustls-tls` feature (no native-tls), so TLS terminates through the rustls + webpki-roots stack. `webpki-roots` is the Mozilla CA bundle re-published under the Community Data License Agreement — Permissive 2.0.

**Why we accept it:**

- The licence is a permissive data-distribution licence (analogous to MIT for code), explicitly compatible with redistribution under our Apache-2.0 project licence.
- It only governs the *root certificate bundle data*, not source code that we modify.
- The chain is unavoidable in the modern rustls ecosystem without bringing in `native-tls` (which we explicitly rejected for security/portability reasons).

**Allowed in** `deny.toml` `[licenses] allow = [..., "CDLA-Permissive-2.0", ...]` with an in-file comment pointing here.
