# RELIX-7 — SOL Runtime Semantics

**Status:** Frozen target. Alpha implements synchronous `remote_call` only (SIMP-001).

## 7.1 Responsibilities

SOL is the orchestration language for cross-node interactions. The runtime defines what SOL programs may do, how they execute, and what guarantees they receive. The runtime = the VM + the bytecode it executes + the yield instructions + the contract with the Flow Coordinator (RELIX-3).

## 7.2 Invariants

1. SOL execution is deterministic given a fixed event log.
2. Every external interaction is a yield; SOL has no other way to affect the world.
3. Flow-local state is private to the flow.
4. Concurrency within a flow is structured (no free-spawn).
5. SOL bytecode for an in-flight flow does NOT change underneath it.

## 7.4 Yield Opcodes (target)

| Opcode | Triggers | Returns |
|---|---|---|
| `yield_call` | Unary RPC | `Result<T, RelixError>` |
| `yield_stream_open` | Stream open | `Result<StreamHandle, _>` |
| `yield_stream_next` | Stream chunk read | `Result<Option<T>, _>` |
| `yield_stream_send` | Chunk write | `Result<(), _>` |
| `yield_stream_close` | Close | `Result<(), _>` |
| `yield_approval_wait` | Resume from approval | `Result<ApprovalDecision, _>` |
| `yield_timer` | Sleep / scheduled wakeup | `Result<(), _>` |
| `yield_time_now` | Deterministic wall clock | `Timestamp` |
| `yield_random` | Deterministic random bytes | `[u8; N]` |
| `yield_parallel_join` | Await concurrent yields | Vec of results |

## 7.6 Deterministic Restrictions

SOL programs MUST NOT: read wall clock directly, generate randomness directly, access env/filesystem/globals, use FP whose rounding mode varies, iterate maps in hash-randomized order, spawn native threads, catch runtime panics. Enforcement: compile-time + runtime defense-in-depth.

## 7.8 Capability Invocation (target)

`Memory.search(query="...")` compiles to:
1. Compile-time CDDL validation.
2. Build args CBOR.
3. Emit `yield_call`.
4. Suspend; coordinator handles RPC; result supplied on resume.

## 7.13 Concurrency

Single-threaded per flow. Concurrent calls via `parallel { ... }` compile to multiple yields with `yield_parallel_join`. No free-spawn.

## 7.15 VM Guarantees

- Deterministic replay (same log → same execution path).
- Durable progress (successful effects survive crashes).
- Bounded replay cost (with snapshots).
- Flow isolation.

## 7.16 VM Does NOT Guarantee

- Exactly-once side effects under all conditions (idempotency-key responsibility per-capability).
- Real-time bounds.
- Cross-flow ordering.
- Cross-flow consistency beyond what mediating nodes provide.

---

## Alpha Implementation Notes

Alpha ships:
- Reuses OpenPrem SOL VM (`crates/relix-runtime/src/sol/`) verbatim.
- Adds one new bytecode instruction: `Inst::RemoteCall { peer_idx, method_idx, args_slot }`.
- VM dispatches `RemoteCall` to a callback registered by the coordinator; the callback performs the RPC synchronously (blocks VM thread) and returns the result on the operand stack.
- Stream consumption uses a similar synchronous yield mechanism but accepts chunked results; each chunk delivery returns a `Some(payload)` until terminal `None`.
- No `parallel { }`, `try/catch`, `Time.now`, `Random.bytes`, or `?` operator in alpha SOL.
- Restrictions: alpha SOL flows MUST NOT use any constructs not yet implemented; the analyzer is unchanged from OpenPrem, so attempts to use undeclared functions fail at compile time.

Alpha SOL flows live in `flows/*.sol` and are loaded by the controller per `configs/*.toml` `[session.<name>] source = "..."`.

Yield/replay-equivalence (RELIX-7 §7.15) is partial in alpha — see SIMP-001 and SIMP-008.
