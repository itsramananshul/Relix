# Audit Query Runbook

How to investigate "what did identity X do" or "what happened on flow Y" using Relix audit and flow logs.

## Where Audit Lives

Audit records are written on the responding node, never centralized. For a query that spans multiple nodes, you fan out: query each responder, join by `request_id` or `trace_id`.

Per-node audit log path: `<data_dir>/audit.log` (default `<data_dir>` = `~/.relix/<node-name>/`).

Per-flow event log path: `<data_dir>/flows/<flow_id>.log`.

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
    --flow ~/.relix/web-bridge/flows/F.log \
    --human
```

Prints a readable trace: `FlowStarted`, each `RemoteCallIssued` + matched `RemoteCallCompleted`, stream chunks, terminal state.

### "Verify a flow log hasn't been tampered with"

```sh
cargo run -p relix-flow-inspect -- \
    --flow ~/.relix/web-bridge/flows/F.log \
    --replay-verify
```

Walks the hash chain, verifies each event's signature, prints `INTEGRITY OK` or detailed failure.

### "Who denied this request?"

If an RPC returned `policy_denied`, the audit record on the responder includes `policy_decision: deny` plus the matched policy rule name:

```sh
cargo run -p relix-flow-inspect -- \
    --audit ~/.relix/memory-node/audit.log \
    --filter 'request_id=="<rid>"' \
    --decisions-only
```

## Cross-Node Investigation

For an incident touching multiple nodes, the join key is `request_id`:

1. Find the originating request in the web bridge's audit (first hit for that user/time window).
2. Take its `request_id`.
3. Grep that `request_id` across every other node's audit log.
4. Reconstruct timeline.

The `trace_id` is also propagated for distributed tracing across nested calls (e.g., chat-flow's outbound `ai.chat` and `memory.write_turn` share a trace_id).

## Compliance: "Did identity X ever call sensitive method Y?"

```sh
for node in memory ai tool web-bridge; do
    cargo run -p relix-flow-inspect -- \
        --audit ~/.relix/$node/audit.log \
        --filter 'caller=="<X>" && method=="<Y>"' \
        --decisions-only
done
```

Empty output across all nodes = identity never invoked the method. Each record across nodes = chronological trail.

## Tamper Detection

The audit log is hash-chained. If a record is added, modified, or removed:

```sh
cargo run -p relix-flow-inspect -- \
    --audit ~/.relix/<node>/audit.log \
    --verify-chain
```

Reports the first chain-break offset. **A chain break is a P0 incident** — the responder's audit cannot be trusted past that point.

## Limits (Alpha)

- No central audit aggregator. Cross-node queries require manual fan-out (a one-line script suffices).
- No `audit.query` capability yet (planned for Gate 3).
- No retention/archival policy enforced — logs grow indefinitely; rotate manually.
- No structured query language; `--filter` accepts a small expression DSL only.

## Escalation

If a query reveals:
- Unauthorized successful access ⇒ rotate the affected identity, review policy.
- Audit chain break ⇒ P0; preserve the file; investigate intrusion or storage corruption.
- Missing records for a call that the caller knows succeeded ⇒ check disk-full / fsync failures on the responder.
