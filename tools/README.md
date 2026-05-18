# Tools

Operator/developer tools live as crates under `crates/`, not under `tools/`. This directory holds non-crate tooling helpers and is currently a placeholder.

Active tools:

- `crates/relix-cli` — identity / ping / inspect.
- `crates/relix-flow-inspect` — flow log + audit log reader, hash-chain verifier.

Future:

- `crates/relix-capabilities-diff` — manifest version-bump enforcement (Gate 2).
- `crates/relix-policy-diff` — policy change-impact analysis (Gate 3).
- `crates/relix-bundle-explorer` — debug tool for signed bundles (Gate 2).
