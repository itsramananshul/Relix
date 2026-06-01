# SOL Runtime — Integration Analysis (M6)

This document maps the ported OpenPrem SOL runtime, explains how `remote_call` integrates without rewriting the VM, and records the alpha simplifications M6 introduces.

## 1. Where the runtime lives

The full OpenPrem SOL toolchain — lexer, parser, semantic analyzer, bytecode codegen, and stack-based VM — was ported verbatim from `Apps/INFRA/open-prem-main/src/sol/` into `crates/relix-runtime/src/sol/`. Only the standalone `main.rs` (a CLI entry point for OpenPrem's `sol` binary) was dropped; `cli.rs` (a small argument parser used by it) is unused-but-kept until M11 cleanup. All other files are unchanged at port time so the diff against upstream is reviewable.

```
crates/relix-runtime/src/sol/
  mod.rs           # pub mod bytecode; pub mod analyzer; ...
                   # exports DEFAULT_MAX_STEPS, MAX_STEPS_CEILING, SolError,
                   # CompiledFlow, compile_source_with_directives, ...
  lexer.rs         (453 LOC)   Token / source → Token stream
  parser.rs        (1362 LOC)  Tokens → Program AST (includes list/map/try/for)
  analyzer.rs      (852 LOC)   Symbol table + type checking
  bytecode.rs      (1037 LOC)  AST → Vec<Inst>; Inst enum (all opcodes including
                               RemoteCall, RemoteCallStream, Try/Catch, List/Map)
  vm.rs            (1773 LOC)  Stack-based VM; pub fn step() / run(); fuel budget;
                               heap; try-handler stack; fault/raise_malformed path
  init.rs          (28  LOC)   Pipeline: sources → Vec<VM>
  util.rs          (111 LOC)
  cli.rs           (129 LOC, unused; kept for upstream parity)
```

The LOC counts above are current as of v0.4.1. They have grown
substantially from the initial M6 port as Relix-specific extensions
(fuel budget, try/catch, list/map opcodes, RemoteCallStream,
last_confidence) were layered on top.

## 2. VM model

Pure stack machine. Per-VM state:

| Field          | Type                  | Purpose                                |
|----------------|-----------------------|----------------------------------------|
| `stack`        | `Vec<u64>`            | Operand stack — every value is a `u64`. |
| `heap`         | `Vec<HeapObject>`     | String / Struct / Array store; values on the operand stack referencing the heap are `u64` indices. |
| `call_stack`   | `Vec<Frame>`          | SOL-function call frames.              |
| `inst_ptr`     | `usize`               | Instruction pointer into the program.  |
| `fp`           | `usize`               | Frame pointer (operand-stack base for the current call). |
| `program`      | `Vec<Inst>`           | Compiled bytecode.                     |
| `done`         | `bool`                | Halt flag (program reached its end).   |

`HeapObject::String(String)` carries Rust UTF-8 strings.

The exec loop is `pub fn step(&mut self) -> Option<u64>`. It returns `None` while still running, `Some(value)` when the program terminates. `run()` is the obvious driver. No async; no yield.

## 3. Instruction set (relevant subset)

```
PushConst(Ast)              # immediate value (int / float / char / bool / string-ref)
LoadLocal(isize)            # push local at fp+offset
StoreLocal(isize)           # pop into local at fp+offset
Pop / Dup
Int* / Float* / Char*       # arithmetic + comparisons
LogOr / LogAnd / LogNot
BitXor / BitAnd / BitOr / ...
NewStruct(n) / GetField(i) / SetField(i)
NewArray / ArrayLen / GetElem / SetElem
ConcatStr / EqStr
Jump(t) / JumpFalse(t)
Call(target, argc)          # SOL-function call
Ret / RetVal
PrintInt / PrintFloat / PrintChar / PrintString
```

`Inst::PushConst(Ast::ExprString(s))` pushes a string by allocating a `HeapObject::String(s)` and pushing its heap index.

## 4. Stack conventions for builtins

The existing `Call(target, argc)` opcode expects the caller to push `argc` args left-to-right, then jump. We follow the same convention for `RemoteCall`: caller pushes three string heap-refs (`peer`, `method`, `arg`) in order, then a single `RemoteCall` opcode pops them, performs the call, and pushes the result (a heap-ref to a new `HeapObject::String`).

## 5. How `remote_call` integrates cleanly

The cleanest integration without rewriting any of the OpenPrem layers:

| Layer    | Change                                                                                              |
|----------|-----------------------------------------------------------------------------------------------------|
| **lexer**    | None. `remote_call` is a regular identifier.                                                  |
| **parser**   | None. `remote_call("x", "y", "z")` is already a valid `Ast::ExprCall { name, args }`.          |
| **analyzer** | One small hook: treat `remote_call` as a known built-in returning `string`, taking three strings, so we don't error with "undefined function." |
| **bytecode (codegen)** | When emitting a call whose callee name is `remote_call`, emit code for the three args + a single new `Inst::RemoteCall` opcode (replacing the usual `Inst::Call`). |
| **vm**   | One new arm in `step()` for `Inst::RemoteCall`: pop three heap-string refs, dispatch via a registered `Arc<dyn RemoteCallDispatcher>` held on the VM, push the response string. |

The string-table aspect of M6/Step 2 ("peer name from string table, method name from string table") is satisfied implicitly: the args live as `HeapObject::String` in the VM heap, so the dispatcher reads them out of the heap when handling the opcode. No separate string table is introduced — the heap is already the only string store the VM has.

## 6. Host-call architecture

A new trait lives in `crates/relix-runtime/src/sol/dispatcher.rs`:

```rust
pub trait RemoteCallDispatcher: Send + Sync {
    /// Synchronous from the VM's perspective. Returns response bytes or a
    /// typed error. Implementations may block on async I/O internally.
    fn remote_call(
        &self,
        peer_alias: &str,
        method: &str,
        arg: &[u8],
    ) -> Result<Vec<u8>, RemoteCallError>;
}
```

The VM holds `Option<Arc<dyn RemoteCallDispatcher>>` (None = no remote calls permitted; a flow that hits `RemoteCall` without a dispatcher errors out cleanly). The default `Vm::new()` / `Vm::from(&bytecode)` constructors leave it `None`; a new `Vm::with_dispatcher(...)` builder attaches it.

The production implementation lives outside the SOL module, in `relix-runtime::controller_runtime`, and wraps the `transport::rpc::Client` plus the caller's identity bundle. The implementation is responsible for:

1. Looking up the peer alias against a `peers: BTreeMap<String, PeerAddr>` (configured via `[peers]` in TOML).
2. Building a `RequestEnvelope` (RELIX-1 §1.4) carrying the caller's `IdentityBundle`, `method`, `args`, deadline.
3. Sending via `Client::call(peer_id, envelope_bytes)` after establishing the connection.
4. Decoding the `ResponseEnvelope` and returning the `Ok(body)` payload or a `RemoteCallError` derived from the error envelope.
5. Emitting `RemoteCallIssued` / `RemoteCallCompleted` / `RemoteCallFailed` events to the per-flow event log (M6/Step 5).

Because the VM is sync and the dispatcher's underlying RPC client is async, the impl uses `tokio::task::block_in_place(|| Handle::current().block_on(future))` so the VM thread blocks safely on the multi-threaded runtime without poisoning other tasks. This is the alpha simplification recorded as **SIMP-001** in `specs/alpha-simplifications.md` (sync `remote_call`); the durable yield model from RELIX-7 lands at Gate 2.

## 7. Deterministic-execution concerns

The VM's bytecode execution is deterministic in the operand-stack sense (no wall clock, no RNG inside SOL programs). `RemoteCall` introduces a single source of non-determinism: the remote node's response. For replay-equivalence (RELIX-7 §7.15 target — SIMP-008), the dispatcher's `RemoteCallCompleted` event records the *result bytes*, and a future replay-mode dispatcher would return logged bytes instead of re-issuing the RPC. The alpha does not implement replay mode (SIMP-008); event logs are structural (RELIX-3 invariants) but the VM is not yet replay-driven. The schema permits the upgrade without breaking older flow logs.

`Time.now`, `Random.bytes`, and other deterministic-replay primitives are out of M6 scope.

## 8. Alpha simplifications introduced by M6

Authoritative copies live in `specs/alpha-simplifications.md`:

- **SIMP-014** — synchronous dispatcher. M6/S4 implementation uses `tokio::task::spawn_blocking` for the VM thread and `Handle::current().block_on(...)` inside the dispatcher (NOT `block_in_place`, which would panic on the current-thread runtime; the spawn_blocking pattern is the tokio-recommended way to mix sync and async). Requires a multi-threaded tokio runtime, which both `relix-controller` and `relix-cli` use.
- **SIMP-015** — client-side flow execution. SOL flows compile and execute in `relix-cli flow-run`; the runner's libp2p PeerId becomes the originating peer for outbound RPCs. The caller's AIC still flows through every `RequestEnvelope`, so responder-side policy decisions are unchanged.
- **SIMP-016** — UTF-8 string args and returns for `remote_call`. The alpha `node.health` body is rewritten to a multi-line `key=value\n` text format to interoperate cleanly with this constraint.
- **SIMP-017** — peer aliases via a flat `peers.toml` file (`--peers configs/peers.toml`). Signed-manifest gossip per RELIX-5 lands at Gate 2.

## 9. What stays out of scope for M6

- No `parallel { }` blocks — calls are strictly sequential.
- No `Time.now` / `Random.bytes` capability surfaces yet.
- No replay-mode VM execution.
- No SOL-side typed structs across the wire — strings only.

These remain documented as future work in `specs/alpha-simplifications.md` and `specs/RELIX-7-sol.md`.

---

## 10. Relix extensions layered on top of the M6 baseline

The sections below document the Relix-specific additions to the ported
OpenPrem runtime. None of these modified the original upstream files —
they were added in separate passes (F2, F5–F8, F11, P6, RELIX-2,
RELIX-7.19).

### 10.1 Fuel budget / `#steps` directive (P6)

Every execution has an instruction budget controlled by two constants
in `sol/mod.rs`:

```rust
pub const DEFAULT_MAX_STEPS: u64 = 100_000;
pub const MAX_STEPS_CEILING: u64 = 10_000_000;
```

The fuel check fires **before** each instruction. When the counter
reaches zero the VM returns `SolError::FuelExhausted { steps_taken }`
rather than executing a final instruction.

`compile_source_with_directives(source, default_max_steps)` honours a
leading `#steps N` directive in the source (wins over the caller's
default; clamped to `MAX_STEPS_CEILING`). `compile_source` strips the
directive for backward compatibility and does not apply it.

The YAML frontend always passes `DEFAULT_MAX_STEPS`; the `#steps`
directive is a `.sol`-only feature.

### 10.2 `try` / `catch` / `rethrow` (F2)

Three new opcodes in `bytecode.rs`:

```
TryEnter(catch_pc)   — push TryHandler { catch_pc, fp_at_enter, stack_len_at_enter }
TryExit              — pop handler on clean path
Rethrow              — re-dispatch via try_dispatch_error(); halt if no handler
```

`try_dispatch_error()` in `vm.rs` pops the nearest handler, restores
`fp`, truncates the operand stack to `stack_len_at_enter`, and jumps to
`catch_pc`. Malformed-bytecode faults go through `raise_malformed` and
bypass the try-handler stack entirely — they are always terminal.

### 10.3 List and map heap objects (F5–F8, F11)

`HeapObject` now has two additional variants:

```rust
HeapObject::List(Vec<u64>)
HeapObject::Map(Vec<(String, u64)>)
```

`Map` is a `Vec`-of-pairs (not a `HashMap`) so iteration is
insertion-order deterministic. The full set of list/map opcodes added:

`PushList(n)`, `PushMap(n)`, `ListLen`, `ListGet`, `ListPush`,
`ListContains`, `ListJoin`, `ListSplit`, `MapGet`, `MapSet`, `MapHas`,
`MapKeys`, `MapLen`, `MapDel`, `ListGetList` (F11), `MapGetMap` (F11).

`list_join` and `list_contains` stringify nested objects via
`heap_display`: nested lists become `|`-separated, nested maps become
`k=v;`-separated. `ListGetList` and `MapGetMap` return a typed heap
reference and produce a catchable `VM_ERROR_SENTINEL` (kind `mesh_error`)
when the element is the wrong type or out of bounds.

### 10.4 `RemoteCallStream` (RELIX-2)

A second remote-call opcode that calls `dispatcher.remote_call_stream`
instead of `dispatcher.remote_call`:

```rust
Inst::RemoteCallStream
```

Stack contract is identical to `RemoteCall` (push `peer`, `method`,
`arg`; pop in reverse order). The dispatcher's `remote_call_stream`
method opens a `/relix/rpc/stream/1` substream and fires the VM's
`chunk_observer` (wired via `VM::with_chunk_observer`) once per
arriving frame. The VM still collects the whole body and pushes a
single result string — streaming is observable only by the external
chunk observer. The default trait implementation falls back to a
single `remote_call`.

### 10.5 `last_confidence` register (RELIX-7.19)

```rust
Inst::LoadLastConfidence
```

`VM` carries a `last_confidence: f32` field (default `1.0`). It is
updated either by the host calling `VM::set_last_confidence(v)` or,
in production, via a shared `LastConfidenceCell` (atomic). The opcode
pushes the value widened to `f64` bits for the VM's float slot. The
builtin `last_confidence() -> float` calls this opcode.
