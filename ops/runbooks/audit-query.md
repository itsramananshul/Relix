# Audit Query Runbook

> Version 0.4.1

How to investigate "what did identity X do" or "what happened on
flow Y" using Relix audit and flow logs. Covers the per-node CBOR
audit log, per-flow event log, and the tenant-partitioned SQLite
mirror (GAP 23C).

## Where Audit Lives

Audit records are written on the responding node, never centralized.
For a query that spans multiple nodes, fan out: query each
responder, join by `request_id` or `trace_id`.

Per-node audit log path:
```
~/.relix/<node-name>/audit.log
```

Per-flow event log path:
```
~/.relix/flow-runner/flows/<flow_id>.log
```

(Both paths use `$RELIX_DATA_DIR` as the base when that env var is
set, falling back to `~/.relix`.)

Tenant partition mirror (when `[audit] partition_by_tenant = true`):
```
~/.relix/<node-name>/audit-partition.db
```

## Quick Queries

### "What did Alice do in the last hour?"

```sh
# On each node:
cargo run -p relix-flow-inspect -- \
    --audit ~/.relix/memory-node/audit.log \
    --filter 'caller=="alice"' \
    --since '1h ago'
```

Repeat for each node. Records are joinable by `request_id`.

### "Show me everything that happened in flow F"

```sh
cargo run -p relix-flow-inspect -- \
    --flow ~/.relix/flow-runner/flows/F.log \
    --human
```

Prints a readable trace: `FlowStarted`, each `RemoteCallIssued` +
matched `RemoteCallCompleted`, stream chunks, terminal state.

### "Verify a flow log hasn't been tampered with"

```sh
cargo run -p relix-flow-inspect -- \
    --flow ~/.relix/flow-runner/flows/F.log \
    --replay-verify \
    --signer-key dev-keys/flow-runner.key
```

Walks the hash chain, verifies each event's signature against the
supplied owner signing key, prints `INTEGRITY OK` or detailed
failure.

### "What happened in this flow, end to end?"

```sh
cargo run -p relix-flow-inspect -- --flow <path> --human
```

`--human` mode prints each event as a header line plus its payload
key=value lines, indented:

```text
seq=2   ts=... kind=RemoteCallCompleted  (18 ms)
    peer=memory
    method=memory.recent_for_session
    request_id=...
    latency_ms=18
    body_bytes=...
```

The `(N ms)` annotation is pulled from the payload's `latency_ms=`
line and is present on `RemoteCallCompleted` / `RemoteCallFailed`
events.

### "Pull only this trace from a responder audit"

```sh
cargo run -p relix-flow-inspect -- --audit <path> --trace <trace_id_hex>
cargo run -p relix-flow-inspect -- --audit <path> --rid   <request_id_hex>
```

Combine with `--human` for the indented form. The header reports
`audit records: N (filtered from M)` when a filter is active.

### "Who denied this request?"

If an RPC returned `policy_denied`, the audit record on the
responder includes `policy_decision: deny` plus the matched rule
name:

```sh
cargo run -p relix-flow-inspect -- \
    --audit ~/.relix/memory-node/audit.log \
    --filter 'request_id=="<rid>"' \
    --decisions-only
```

## Tenant Partition Queries (GAP 23C)

Requires the responding node to run with `[audit] partition_by_tenant = true`.
When enabled, every admission decision is mirrored to
`audit-partition.db` **before** the canonical CBOR record is
finalised. The partition is an additive queryable view; the CBOR
chain is the source of truth.

### "List all tenants seen by a node"

```sh
relix-cli cap --peer <node-name> -- node.audit.tenant_list
```

Returns one tenant id per line + `count=N`. Returns `count=0`
when the partition is disabled.

### "Get the most recent records for tenant acme"

```sh
relix-cli cap --peer <node-name> -- node.audit.tenant_recent "acme|50"
```

Arg format: `<tenant_id>|<limit>` (pipe-delimited). `limit`
default 100, clamp `[1, 1000]`. Returns JSON:

```json
{
  "tenant_id": "acme",
  "count": 50,
  "rows": [
    {
      "ts_secs": 1748900000,
      "request_id": "deadbeef...",
      "tenant_id": "acme",
      "caller_name": "alice",
      "method": "ai.chat",
      "policy_decision": "allow:rule_ai_users",
      "status": "ok",
      "error_kind": null,
      "latency_ms": 142
    }
  ]
}
```

Results are newest-first (by `ts_secs DESC`).

### "How were tenants sanitised?"

Tenant ids are normalised before storage: ASCII alphanumeric + `_`
are kept; every other character becomes `_`; `None`/empty becomes
`"default"`. To look up a tenant with special characters (e.g.
`"acme-corp"`), query with `"acme_corp"`.

## Cross-Node Investigation

For an incident touching multiple nodes, the join key is
`request_id`:

1. Find the originating request in the web bridge's audit (first
   hit for that user/time window).
2. Take its `request_id`.
3. Grep that `request_id` across every other node's audit log.
4. Reconstruct timeline.

The `trace_id` is also propagated for distributed tracing across
nested calls (e.g., chat-flow's outbound `ai.chat` and
`memory.write_turn` share a trace_id).

Note: `ResponseEnvelope.aid` (the audit id in each response) is a
server-minted UUIDv4 that is **not** equal to `request_id`. Use
`request_id` for cross-correlation across caller and responder
logs; `aid` is for identifying a specific audit row on the
responder side.

## Compliance: "Did identity X ever call sensitive method Y?"

```sh
for node in memory ai tool web-bridge; do
    cargo run -p relix-flow-inspect -- \
        --audit ~/.relix/$node/audit.log \
        --filter 'caller=="<X>" && method=="<Y>"' \
        --decisions-only
done
```

Empty output across all nodes = identity never invoked the method.

For tenant-scoped compliance, also query the partition mirror on
each node if `partition_by_tenant = true`:

```sh
relix-cli cap --peer <node> -- node.audit.tenant_recent "<tenant>|1000"
```

## Tamper Detection

The audit log is hash-chained. If a record is added, modified, or
removed:

```sh
cargo run -p relix-flow-inspect -- \
    --audit ~/.relix/<node>/audit.log \
    --verify-chain
```

Reports the first chain-break offset. **A chain break is a P0
incident** — the responder's audit cannot be trusted past that
point.

Key-rotation recovery: if the chain fails because records were
written by a different key (legitimate key rotation), the old log
file will have been quarantined automatically as
`audit.log.quarantined-<ms>` on the node's next boot. If the chain
break claims the current key, it is a tamper signal and the node
will not recover automatically.

## Audit Surfaces Summary

| Surface | Path | Query method |
|---|---|---|
| Per-node CBOR audit log | `~/.relix/<node>/audit.log` | `relix-flow-inspect --audit` |
| Per-flow CBOR event log | `~/.relix/flow-runner/flows/<flow_id>.log` | `relix-flow-inspect --flow` |
| Coordinator task chronicle | coordinator SQLite | `/v1/tasks/:id/events`, `task.events` cap |
| Tenant partition mirror | `~/.relix/<node>/audit-partition.db` | `node.audit.tenant_list`, `node.audit.tenant_recent` caps |
| Evidence store | `~/.relix/<node>/evidence.db` | `execution.evidence` cap |
| PII chronicle | `~/.relix/<node>/pii_events.sqlite` | `pii.recent_events`, `pii.scan_stats` caps |

## Limits

- No central audit aggregator. Cross-node queries require manual
  fan-out (a one-line script suffices).
- Tenant-partitioned SQLite mirror is shipped; a unified
  cross-node query language is deferred.
- No retention/archival policy enforced — logs grow indefinitely;
  rotate manually.
- No structured query language on the CBOR logs; `--filter` accepts
  a small expression DSL only.
- The tenant partition mirror is advisory (not signed); the CBOR
  audit chain is the authoritative record.

## Escalation

If a query reveals:
- Unauthorized successful access ⇒ rotate the affected identity,
  review policy.
- Audit chain break ⇒ P0; preserve the file; investigate intrusion
  or storage corruption.
- Missing records for a call that the caller knows succeeded ⇒
  check disk-full / fsync failures on the responder.
- Tenant ids unexpectedly normalised to `"default"` ⇒ check that
  callers are including `tenant_id` in their request envelopes and
  that `partition_by_tenant = true` is set on the node.
