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

### What is actually shipped in v0.4.1

The target spec above describes `yield_call`, `yield_stream_*`,
`yield_approval_wait`, `yield_timer`, etc. **None of these opcodes
exist in the current implementation.** The shipped opcodes are:

- `Inst::RemoteCall` — synchronous unary RPC (the shipped form of
  what this spec calls `yield_call`). The VM blocks the thread and
  returns one result buffer.
- `Inst::RemoteCallStream` (RELIX-2) — same synchronous contract from
  the SOL author's perspective; fires a chunk observer on the host
  side as each frame arrives via `/relix/rpc/stream/1`.

The yield/suspend/resume model described in §7.4 and §7.8 is a
**long-term target** that has not yet been built. Readers using this
spec as a current implementation reference should treat every opcode
name in §7.4 as a planned future name, not a current one.

Additional Relix extensions shipped on top of the OpenPrem port:

- `try` / `catch` / `rethrow` (`TryEnter`, `TryExit`, `Rethrow`) — F2.
- List and map heap objects + all `list_*` / `map_*` opcodes — F5–F8, F11.
- Fuel budget: `DEFAULT_MAX_STEPS = 100_000`, `MAX_STEPS_CEILING = 10_000_000`,
  `#steps N` directive, `SolError::FuelExhausted` — P6.
- `last_confidence()` / `Inst::LoadLastConfidence` — RELIX-7.19.

### Invariant 1 caveat

§7.2 invariant 1 ("SOL execution is deterministic given a fixed event
log") is **not yet enforced**. There is no replay-infrastructure in the
current code (SIMP-008). Event logs are written and can be inspected,
but the VM is not replay-driven. This invariant is a target, not a
current guarantee.

Alpha SOL flows live in `flows/*.sol` and are loaded by the controller per `configs/*.toml` `[session.<name>] source = "..."`.

See SIMP-001 and SIMP-008 in `specs/alpha-simplifications.md`.
