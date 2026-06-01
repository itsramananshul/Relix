# RELIX-3 — Event Log + Flow Coordinator

**Version:** 0.4.1 | **Status:** Frozen target. Alpha implements core append + chain + audit indexing; defers snapshots (SIMP-005) and full replay-equivalence (SIMP-008).

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

Fields: `flow_id` (16-byte), `event_seq` (u64), `ts` (tag(1); ordering/ops only — not consumed by replay), `kind` (snake_case string; see §3.4), `payload` (CBOR), `prev_hash` (32 bytes), `signature` (64 bytes Ed25519).

**Disk framing:** each record is preceded by a 4-byte big-endian length field; the CBOR record bytes follow immediately. This framing is used by both the event log and the audit log.

> **Alpha wire note:** The frozen-target spec describes `type` as a `u8` integer. The implemented wire format serialises the event kind as a snake_case string (e.g. `"flow_started"`), not a numeric discriminant. The field is named `kind` in the Rust struct. Callers reading raw log files MUST expect a string, not an integer.

## 3.4 Event Types (stable enum, ≥ 1024 reserved)

**Target set (19 types):**
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

**Alpha wire values** — the 7 variants implemented in v0.4.1, serialised as snake_case strings:

| Wire string | Meaning |
|---|---|
| `"flow_started"` | Flow execution began. |
| `"remote_call_issued"` | Outbound RPC issued (LOG-BEFORE-ACT). |
| `"remote_call_completed"` | Outbound RPC returned successfully. |
| `"remote_call_failed"` | Outbound RPC returned an error or timed out. |
| `"stream_chunk_received"` | One chunk received on a streaming call. |
| `"flow_completed"` | Flow reached a terminal success state. |
| `"flow_failed"` | Flow reached a terminal failure state. |

All other target-set types are declared but not currently emitted by the runtime. Log readers MUST tolerate unknown string values (forward-compatibility).

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

## Alpha Implementation Notes (v0.4.1)

Alpha ships:
- Append-only log per flow with hash chain + Ed25519 signature.
- Disk framing: 4-byte big-endian length prefix + CBOR record (see §3.3).
- Event kinds: 7 variants implemented — `flow_started`, `remote_call_issued`, `remote_call_completed`, `remote_call_failed`, `stream_chunk_received`, `flow_completed`, `flow_failed`. Wire format: snake_case strings (not u8 integers).
- Sequence overflow at `u64::MAX` returns `SequenceOverflow` error (not silent wrap).
- On `EventLog::open`: parent directories are created, and the existing chain is replayed and verified before accepting appends.
- Synchronous SOL means no parking — but log-before-act is still honored.
- No snapshots (SIMP-005); replay from `event_seq=0`.
- No `RandomDrawn` / `WallClockRead` yet (SOL doesn't expose them); alpha flows are deterministic by absence of these calls.

Audit records (in `relix-core::audit`) reference flow events by `(flow_id, event_seq)` and live in a parallel append-only audit log per node. Cross-correlation is by `request_id`.
