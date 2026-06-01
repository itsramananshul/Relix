# Tools

Operator/developer tools live as crates under `crates/`, not under `tools/`. This directory holds non-crate tooling helpers and is currently a placeholder.

Active tools:

- `crates/relix-cli` — identity / ping / inspect.
- `crates/relix-flow-inspect` — flow log + audit log reader, hash-chain verifier.

Future:

- `crates/relix-capabilities-diff` — manifest version-bump enforcement (Gate 2).
- `crates/relix-policy-diff` — policy change-impact analysis (Gate 3).
- `crates/relix-bundle-explorer` — debug tool for signed bundles (Gate 2).

---

## `relix-flow-inspect` — flag reference

```
relix-flow-inspect [OPTIONS]

--flow <path>             Read a flow event log (CBOR format)
--audit <path>            Read a signed CBOR audit log
--audit-partition <path>  Read the SQLite per-tenant audit partition mirror
--replay-verify           Hash-chain + signature verification (flow only)
--signer-key <path>       32-byte raw Ed25519 signing key file (for --replay-verify)
--human                   Indented multi-line output with latency_ms extraction
--trace <hex>             Filter audit records by trace_id (hex; audit only)
--rid <hex>               Filter audit records by request_id (hex; audit only)
--tenant <id>             Filter partition rows to one tenant (requires --audit-partition)
--all-tenants             Print all tenants grouped with separator; requires interactive confirmation
--multi-tenant-mode       Reject no-filter run as an error (enforce --tenant/--all-tenants)
--yes                     Skip --all-tenants confirmation prompt (for non-interactive pipelines)
--limit <n>               Cap rows per tenant from the partition store; default 1000
```

Exactly one of `--flow`, `--audit`, `--audit-partition` must be provided.

`--tenant` and `--all-tenants` are mutually exclusive; both require `--audit-partition`.

`--replay-verify` prints `INTEGRITY OK`, record count, and `next_seq`; requires
`--signer-key`.

`--all-tenants` confirmation prompt: accepts only `yes` (case-insensitive, trimmed);
anything else prints `Aborted; no records read.` and exits cleanly.

`--multi-tenant-mode` with no tenant filter exits with:
`In multi-tenant mode you must specify --tenant <id> or --all-tenants.`

Signing key bytes are held in `Zeroizing<Vec<u8>>` + `Zeroizing<[u8;32]>` and
wiped on drop.

See [`docs/audit-trails.md`](../docs/audit-trails.md) for usage recipes and
log-correlation guidance.
