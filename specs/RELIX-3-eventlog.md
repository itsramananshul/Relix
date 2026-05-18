# RELIX-3 — Event Log + Flow Coordinator

**Status:** Frozen target. Alpha implements core append + chain + audit indexing; defers snapshots (SIMP-005) and full replay-equivalence (SIMP-008).

## 3.1 Responsibilities

The Event Log is the per-flow, append-only, hash-chained, signed record of every externally-observable event experienced by a SOL flow. The Flow Coordinator schedules VM execution, parks/wakes flows on external events, performs replay-based recovery, and feeds events into audit.

## 3.2 Invariants

1. **Log-before-act:** No side-effecting external call may issue without first durably writing its issuance event.
2. **Monotonic sequencing:** `event_seq` is strictly increasing per flow.
3. **Hash-chained:** Each event's `prev_hash` = BLAKE3-256 of prior event's full encoding. First event's prev_hash = 32 zero bytes.
4. **Signed:** Every event is signed by the owning controller's identity key.
5. **Single owner:** A flow has exactly one owning controller for its lifetime.
6. **Deterministic replay:** Replay of the same log into a fresh VM with the same bytecode produces identical execution.
7. **Audit-equivalence:** The flow event log IS the canonical audit record for events within that flow.

## 3.3 Event Record

Fields: `flow_id` (16-byte), `event_seq` (u64), `ts` (tag(1); ordering/ops only — not consumed by replay), `type` (u8), `payload` (CBOR), `prev_hash` (32 bytes), `sig` (64 bytes Ed25519).

## 3.4 Event Types (stable enum, ≥ 1024 reserved)

```
1  FlowStarted               2  RemoteCallIssued
3  RemoteCallCompleted       4  StreamOpened
5  StreamChunkReceived       6  StreamChunkSent
7  StreamClosed              8  ApprovalRequested
9  ApprovalResolved         10  TimerSet
11 TimerFired               12 TimerCancelled
13 RandomDrawn              14 WallClockRead
15 Snapshot                 16 FlowCancelled
17 FlowFailed               18 FlowCompleted
19 Migrated
```

## 3.5 Sequencing and Durability

Writes MUST be persisted (fsync or equivalent) before the action they describe is issued or any external effect is acknowledged.

## 3.6 Replay

On startup or recovery, for each non-terminal flow:
1. Locate latest `Snapshot` event if any; restore VM state.
2. Replay events after the snapshot in order: completed events supply results to VM directly; issued-but-not-completed park the VM.

## 3.7 Re-Issuance After Crash

If crash between `RemoteCallIssued` and `RemoteCallCompleted`:
- `idempotent` capabilities: re-issue with recorded idempotency key.
- `at_most_once`: do not re-issue; flow parks indefinitely or fails with `uncertain_after_crash`.

## 3.11 Determinism: Time and Randomness

SOL has no direct access to wall clock or RNG. `Time.now()` and `Random.bytes(n)` are yields that log `WallClockRead` / `RandomDrawn` events; replay supplies recorded values.

## 3.12 Cancellation

`FlowCancelled` event causes next yield to resume with `Err(Cancelled)`. Cleanup bounded by `hard_cancellation_deadline` (default 30 s).

---

## Alpha Implementation Notes

Alpha ships:
- Append-only log per flow with hash chain + Ed25519 signature.
- Event types: `FlowStarted`, `RemoteCallIssued`, `RemoteCallCompleted`, `StreamChunkReceived`, `FlowCompleted`, `FlowFailed`. Other types stubbed.
- Synchronous SOL means no parking — but log-before-act is still honored.
- No snapshots (SIMP-005); replay from `event_seq=0`.
- No `RandomDrawn` / `WallClockRead` yet (SOL doesn't expose them); alpha flows are deterministic by absence of these calls.

Audit records (in `relix-core::audit`) reference flow events by `(flow_id, event_seq)` and live in a parallel append-only audit log per node. Cross-correlation is by `request_id`.
