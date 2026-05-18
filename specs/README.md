# Relix Specifications

This directory is the **source of truth** for Relix's protocols, runtime semantics, and security model. Implementation conforms to specs; specs do not retroactively rationalize implementation.

## Reading Order

1. **`alpha-simplifications.md`** — START HERE if you're working on the current alpha. It documents the deltas between what's shipping this week and the substrate target. Every alpha simplification has a deadline (gate) for resolution.
2. **`identity-employees.md`** — The agents-as-employees identity model. The architectural reason Relix exists in its present form.
3. **`RELIX-1` through `RELIX-8`** — The substrate freeze. The production target.
4. **`threat-model.md`** — Attacker classes, asset classes, mitigations. Updated per gate.

## Substrate Spec Index

| Spec | Topic | Status |
|---|---|---|
| `RELIX-1-rpc.md` | Relix RPC protocol (`/relix/rpc/1`) | Frozen target; alpha implements subset |
| `RELIX-2-stream.md` | Streaming/substream protocol (`/relix/stream/1`) | Frozen target; alpha implements simplified variant |
| `RELIX-3-eventlog.md` | Event log + flow coordinator | Frozen target; alpha implements basics |
| `RELIX-4-bundle.md` | Signed-bundle format (COSE_Sign1-style) | Frozen target; alpha implements simplified envelope |
| `RELIX-5-manifest.md` | Node manifest format | Frozen target; alpha implements minimal |
| `RELIX-6-capability.md` | Capability descriptor format | Frozen target; alpha implements minimal |
| `RELIX-7-sol.md` | SOL runtime semantics | Frozen target; alpha implements synchronous `remote_call` only |
| `RELIX-8-flow.md` | Flow lifecycle model | Frozen target; alpha implements basics |

## Spec Governance

Changes to anything in this directory follow `CONTRIBUTING.md`:

- Filing an RFC issue.
- A 72-hour discussion window (longer for substrate changes).
- An amendment PR that updates the spec, `conformance/` vectors, and `CHANGELOG-SPEC.md`.
- Spec amendments are NOT bundled with implementation PRs.

## What These Specs Are Not

- Not a wishlist. Every listed normative behavior is a target the alpha is converging toward.
- Not a research document. Disputes go through RFC, not silent edits.
- Not a place for product copy. Architecture-only.
