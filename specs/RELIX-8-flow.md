# RELIX-8 — Flow Lifecycle Model

**Status:** Frozen target. Alpha implements basics; defers migration, archival, full cancellation hardening.

## 8.1 Responsibilities

States a flow inhabits, transitions between them, ownership/identity rules, and operational primitives (cancel, archive, replay, version-pin).

## 8.2 Invariants

1. Every flow has exactly one owning controller for its full lifetime.
2. Every flow has an immutable `(definition_id, definition_version)` captured at creation.
3. Identity propagation defaults to least-privilege (flow runs as initiator).
4. Terminal states are terminal — no transitions out of `Completed`, `Failed`, `Cancelled`, `Migrated`.
5. `flow_id` unique within owning controller; globally `(owner_peer_id, flow_id)`.

## 8.3 States

```
Created → Running ⇄ Suspended → Cancelling → Terminal{Completed|Failed|Cancelled|Migrated} → Archived → Purged
```

Each transition is one event in the flow log.

## 8.4 Creation

Triggers: inbound RPC to a handler, scheduled timer, parent flow spawn. Coordinator allocates `flow_id` (UUIDv7), resolves `(definition_id, definition_version)`, writes `FlowStarted`, instantiates VM.

## 8.5 Ownership

Owned by the creating controller. Owning controller has exclusive write access to the flow log and signs every event.

## 8.7 Identity Propagation

Default: outbound calls carry `initiator_identity` (least privilege). Substitution to host identity requires explicit node policy permission; substitutions logged.

## 8.8 Suspension and Resumption

Suspension at every yield. State persisted via event log (and optional snapshot). Resume when awaited event arrives. Long suspensions are first-class (e.g., 72-hour approval waits).

## 8.9 Timeout, Cancellation, Archival

- Timeout: `deadline` passed ⇒ `FlowCancelled{reason: deadline}`.
- Cancellation: authorized principal calls `flow.cancel`; idempotent.
- Cancelling state: next yield raises `Cancelled`; cleanup bounded by `hard_cancellation_deadline`.
- Archival: 90d online default, then cold storage with integrity preserved.

## 8.10 Migration

NOT supported in v1. Documented escape hatch: `migrate_flow` operation re-emits accumulated state into a new flow under a new definition; original terminates with `Migrated`.

## 8.11 Replay

Read-only re-execution from event log; no side effects re-issued. Captured `definition_version` used.

## 8.12 Version Upgrades

In-flight flows continue on their captured `definition_version`. New flows use latest. Old version remains loaded as long as in-flight flows reference it.

---

## Alpha Implementation Notes (v0.4.1)

The target spec above describes a durable yield/suspend/resume model,
90-day archival, migration, and full replay. **None of these are
implemented in v0.4.1.** The body of this spec is a frozen target for
Gate 2+, not a description of current behavior.

What is shipped:

- States: `Created`, `Running`, `Completed`, `Failed`. No `Suspended`
  (SOL execution is synchronous — no parking), no `Cancelling` /
  `Cancelled` flow state, no `Archived` / `Purged` lifecycle.
- Flow IDs: 16-byte random (UUIDv4 — UUIDv7 at Gate 2).
- Ownership: implicit — the controller that handles the trigger RPC
  owns the flow for its duration.
- Identity propagation: implicit — outbound calls within the flow carry
  the original caller's identity (least privilege).
- Three file formats: `.sol` (hand-written), `.sflow` (AST executor),
  `.yml`/`.yaml` (lowered to SOL). All share the same event log format.
- No migration, no archival, no version pinning yet.
- Replay: `relix-flow-inspect --replay-verify` checks event log
  integrity (hash-chain verification); no bytecode re-execution
  (SIMP-008). The replay target in §8.11 is not yet built.

The `flow_id` is captured by `RemoteCallIssued` events, making
cross-node correlation work even though the flow lifecycle is minimal.
