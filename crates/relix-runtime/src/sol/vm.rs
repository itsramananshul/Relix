use crate::sol::bytecode::Inst;
use crate::sol::dispatcher::{RemoteCallDispatcher, RemoteCallError};
use crate::sol::parser::Ast;
use std::io::{self, Write};
use std::sync::Arc;

/// Sentinel returned by `run()` when the program halted due to an unhandled
/// `RemoteCall` failure (or any other runtime error introduced by Relix
/// extensions). Distinguishable from normal SOL exit codes by being u64::MAX.
pub const VM_ERROR_SENTINEL: u64 = u64::MAX;

#[derive(Debug, Clone)]
pub enum HeapObject {
    String(String),
    Struct(Vec<u64>),
    Array(Vec<u64>),
    /// F5: heterogeneous list. Element refs are heap-string
    /// indices in the common case but the VM does not enforce
    /// that — the type is `Vec<u64>` for consistency with
    /// `Array`, and built-ins like `list_join` interpret the
    /// refs as heap-strings at access time.
    List(Vec<u64>),
    /// F7: string-keyed map. Insertion order is preserved so
    /// `map_keys` returns keys deterministically. Values are
    /// raw heap refs; `map_get` interprets them as strings.
    Map(Vec<(String, u64)>),
}

struct Frame {
    return_ptr: usize,
    old_fp: usize,
}

/// One active try-handler. Carries the bytecode address the
/// VM jumps to when the wrapped body fails, plus the frame
/// pointer at the time of `TryEnter` so a failure deep inside
/// nested calls restores the stack correctly before
/// dispatching to the catch.
#[derive(Debug, Clone, Copy)]
struct TryHandler {
    /// First instruction of the catch dispatch block.
    catch_pc: usize,
    /// Frame pointer at TryEnter — restored on dispatch so
    /// the catch block sees the same locals as the enclosing
    /// frame.
    fp_at_enter: usize,
    /// Stack length at TryEnter — anything pushed by the
    /// in-progress try body is unwound before the catch
    /// runs so the dispatch starts with a clean working
    /// stack.
    stack_len_at_enter: usize,
}

pub struct VM {
    stack: Vec<u64>,
    heap: Vec<HeapObject>,
    call_stack: Vec<Frame>,
    inst_ptr: usize,
    fp: usize,
    program: Vec<Inst>,
    done: bool,
    /// P6 — per-execution fuel budget. Each successful
    /// `step()` decrements by 1; reaching 0 halts the VM with
    /// `VM_ERROR_SENTINEL` and sets `last_error` to the
    /// fuel-exhaustion cause. Operators size the budget via
    /// the `[sol] max_steps` config (default
    /// [`crate::sol::DEFAULT_MAX_STEPS`]) or the per-flow
    /// `#steps N` directive. Both are clamped to
    /// [`crate::sol::MAX_STEPS_CEILING`].
    fuel: u64,
    /// P6 — how many instructions we have executed since
    /// construction (or since `with_fuel`). Reported in the
    /// `FuelExhausted` cause + log line so operators can size
    /// budgets sensibly.
    steps_taken: u64,
    /// P6 — operator-supplied flow name for log lines fired
    /// when fuel runs out. `None` falls back to "<sol_vm>".
    flow_name: Option<String>,
    /// P6 — set when fuel ran out on this execution. Drives
    /// `fuel_exhausted_steps()` so the flow_runner can lift
    /// the typed [`crate::sol::SolError::FuelExhausted`] back
    /// out for callers.
    fuel_exhausted_steps: Option<u64>,
    /// Relix extension (M6): optional host-side dispatcher for `Inst::RemoteCall`.
    /// `None` means remote calls are forbidden — encountering `RemoteCall` halts
    /// the VM with a `local_dispatch_error`.
    dispatcher: Option<Arc<dyn RemoteCallDispatcher>>,
    /// Relix extension (M6): structured error from the last failed
    /// `RemoteCall`, if any. Cleared on successful step.
    last_error: Option<RemoteCallError>,
    /// F2: stack of active try-handlers. Pushed by
    /// `Inst::TryEnter`, popped by `Inst::TryExit` (clean
    /// finish) or by the error dispatch (failure).
    try_handlers: Vec<TryHandler>,
    /// RELIX-2 step 4: optional chunk observer wired by the
    /// host BEFORE the VM runs. When set, `Inst::RemoteCallStream`
    /// passes this callback into the dispatcher's
    /// [`RemoteCallDispatcher::remote_call_stream`] call —
    /// each chunk fires the callback synchronously, in
    /// arrival order, BEFORE the VM has finished collecting
    /// the concatenated result. The web bridge uses this to
    /// pipe tokens into an SSE stream while the SOL flow is
    /// still running. None = no observer (a no-op closure is
    /// passed instead).
    chunk_observer: Option<Arc<dyn Fn(&[u8]) + Send + Sync>>,
    /// RELIX-7.19: confidence score of the most recently
    /// completed `remote_call`. The host updates this via
    /// [`VM::set_last_confidence`] after each call (or via a
    /// shared [`crate::confidence::LastConfidenceCell`] if
    /// the host prefers shared storage). Read by the
    /// `last_confidence()` SOL builtin via
    /// [`Inst::LoadLastConfidence`]. Defaults to `1.0` so
    /// flows that read the value before any call see a
    /// neutral score.
    last_confidence: f32,
    /// RELIX-7.19: optional shared confidence cell. When
    /// wired, `Inst::LoadLastConfidence` reads from the cell
    /// instead of `last_confidence` so the dispatcher and the
    /// VM see the same value without per-call updates from
    /// the host.
    last_confidence_cell: Option<crate::confidence::LastConfidenceCell>,
    /// Set mid-instruction when malformed/hostile bytecode hits
    /// a VM-integrity fault — stack underflow, an out-of-range
    /// heap ref or local slot, an out-of-bounds element index,
    /// a bad constant payload, or an oversized allocation. The
    /// VM's `pop()` and the opcode handlers record the cause
    /// here instead of panicking; `step()` checks it after each
    /// instruction and converts it to a clean halt
    /// (`VM_ERROR_SENTINEL` + structured `last_error`). Bytecode
    /// compiled from operator/agent-authored SOL or YAML is
    /// untrusted, so these faults must never abort the
    /// `spawn_blocking` worker.
    fault: Option<String>,
}

impl VM {
    pub fn new() -> Self {
        Self {
            stack: Vec::with_capacity(512),
            heap: Vec::with_capacity(128),
            call_stack: Vec::with_capacity(64),
            inst_ptr: 0,
            fp: 0,
            program: Vec::new(),
            done: false,
            dispatcher: None,
            last_error: None,
            try_handlers: Vec::new(),
            chunk_observer: None,
            last_confidence: 1.0,
            last_confidence_cell: None,
            // P6: default fuel matches the operator-config
            // default; per-flow overrides go through
            // `with_fuel` after construction.
            fuel: crate::sol::DEFAULT_MAX_STEPS,
            steps_taken: 0,
            flow_name: None,
            fuel_exhausted_steps: None,
            fault: None,
        }
    }

    /// P6 — override the per-execution fuel budget. Clamped
    /// to [`crate::sol::MAX_STEPS_CEILING`]; a `fuel` of `0`
    /// is rewritten to `1` (a flow with zero budget is
    /// instantly out of fuel — pointless and confusing). Use
    /// `with_fuel(crate::sol::MAX_STEPS_CEILING)` to lift the
    /// per-execution cap to the hard ceiling.
    pub fn with_fuel(mut self, fuel: u64) -> Self {
        self.fuel = fuel.min(crate::sol::MAX_STEPS_CEILING).max(1);
        self
    }

    /// P6 — operator-supplied flow name for the
    /// fuel-exhaustion log line. Builder-style.
    pub fn with_flow_name(mut self, name: impl Into<String>) -> Self {
        self.flow_name = Some(name.into());
        self
    }

    /// P6 — `Some(steps_taken)` iff the most recent `run()`
    /// halted because fuel ran out. Lets the host lift the
    /// typed [`crate::sol::SolError::FuelExhausted`] back out
    /// (the public VM contract returns `u64` to stay
    /// back-compat with `flow_runner`).
    pub fn fuel_exhausted_steps(&self) -> Option<u64> {
        self.fuel_exhausted_steps
    }

    /// P6 — how many instructions this VM has executed since
    /// construction. Useful for assertions in tests.
    pub fn steps_taken(&self) -> u64 {
        self.steps_taken
    }

    /// P6 — remaining fuel. Exposed for diagnostic surfaces
    /// + tests.
    pub fn fuel(&self) -> u64 {
        self.fuel
    }

    pub fn from(program: &[Inst]) -> Self {
        Self {
            program: program.to_vec(),
            ..Self::new()
        }
    }

    /// Relix extension: attach a `RemoteCallDispatcher` so the VM can execute
    /// `Inst::RemoteCall`. Builder-style.
    pub fn with_dispatcher(mut self, dispatcher: Arc<dyn RemoteCallDispatcher>) -> Self {
        self.dispatcher = Some(dispatcher);
        self
    }

    /// RELIX-7.19: set the confidence score for the most
    /// recently completed `remote_call`. Hosts call this after
    /// every dispatch so `last_confidence()` returns the
    /// latest verdict. Clamped to `[0.0, 1.0]`.
    pub fn set_last_confidence(&mut self, value: f32) {
        self.last_confidence = value.clamp(0.0, 1.0);
    }

    /// RELIX-7.19: read the current confidence reading (the
    /// value that `last_confidence()` would push). Defaults to
    /// `1.0` before any `remote_call` has been made.
    pub fn last_confidence(&self) -> f32 {
        if let Some(cell) = &self.last_confidence_cell {
            return cell.get();
        }
        self.last_confidence
    }

    /// RELIX-7.19: attach a shared confidence cell. When wired,
    /// `Inst::LoadLastConfidence` reads from the cell instead
    /// of the VM-local field — useful when the dispatcher
    /// integration owns the source-of-truth value.
    pub fn with_last_confidence_cell(
        mut self,
        cell: crate::confidence::LastConfidenceCell,
    ) -> Self {
        self.last_confidence_cell = Some(cell);
        self
    }

    /// RELIX-7.19: post-construction setter for the
    /// confidence cell. Same semantics as
    /// [`Self::with_last_confidence_cell`] but consumable
    /// after the VM is built — flow_runner uses this because
    /// the cell is created alongside the dispatcher AFTER the
    /// VM is parked.
    pub fn set_last_confidence_cell(&mut self, cell: crate::confidence::LastConfidenceCell) {
        self.last_confidence_cell = Some(cell);
    }

    /// RELIX-2 step 4: attach a chunk observer. Invoked once
    /// per chunk during `Inst::RemoteCallStream` evaluation,
    /// in arrival order. The web bridge uses this to ship
    /// tokens to an HTTP SSE response BEFORE the VM finishes
    /// collecting the concatenated result. Builder-style.
    pub fn with_chunk_observer(mut self, observer: Arc<dyn Fn(&[u8]) + Send + Sync>) -> Self {
        self.chunk_observer = Some(observer);
        self
    }

    /// Relix extension: the structured error from the last failed `RemoteCall`,
    /// or `None` if the VM has not produced one. Cleared each successful step.
    pub fn last_error(&self) -> Option<&RemoteCallError> {
        self.last_error.as_ref()
    }

    /// Relix extension: resolve a `HeapObject::String` by its heap index.
    /// Used by `flow_runner` after `run()` to surface a SOL flow's return
    /// value (heap-string ref) as a real string.
    pub fn heap_string(&self, idx: u64) -> Option<&str> {
        match self.heap.get(idx as usize) {
            Some(HeapObject::String(s)) => Some(s.as_str()),
            _ => None,
        }
    }

    #[inline]
    fn push(&mut self, val: u64) {
        self.stack.push(val);
    }

    /// Pop the operand stack. On underflow (malformed bytecode)
    /// this records a fault and returns `0` instead of
    /// panicking; `step()` converts the recorded fault into a
    /// clean halt after the instruction finishes. The bogus `0`
    /// is harmless because the VM bails before the result is
    /// observable.
    #[inline]
    fn pop(&mut self) -> u64 {
        match self.stack.pop() {
            Some(v) => v,
            None => {
                self.fault("stack underflow");
                0
            }
        }
    }

    /// Record a VM-integrity fault from malformed bytecode. Only
    /// the first fault per instruction is kept (later faults in
    /// the same instruction are downstream effects of the
    /// first).
    #[inline]
    fn fault(&mut self, cause: impl Into<String>) {
        if self.fault.is_none() {
            self.fault = Some(cause.into());
        }
    }

    /// Bound an operator-controlled allocation request. The size
    /// is checked against the remaining fuel (each element costs
    /// at least as much as a VM step) and an absolute ceiling so
    /// a malformed `size` (e.g. `usize::MAX` popped for
    /// `NewArray`) cannot OOM the `spawn_blocking` worker before
    /// the fuel counter intervenes. Records a fault and returns
    /// `false` when the request is rejected.
    fn check_alloc(&mut self, requested: usize, what: &str) -> bool {
        // Absolute element ceiling, independent of fuel, so a
        // very large fuel budget can't authorize a multi-GB
        // allocation.
        const ALLOC_CEILING: usize = 1 << 24; // 16,777,216 elements
        let budget = (self.fuel as usize).min(ALLOC_CEILING);
        if requested > budget {
            self.fault(format!(
                "{what}: requested {requested} elements exceeds allocation budget {budget}"
            ));
            false
        } else {
            true
        }
    }

    /// Convert a recorded `fault` into a clean, hard halt: set
    /// the structured `last_error` (so `flow_runner` surfaces a
    /// cause exactly as it does for `RemoteCall` failures and
    /// fuel exhaustion) and return the error sentinel. Malformed
    /// bytecode is a VM-integrity fault, so — unlike SOL-level
    /// `RemoteCall` errors — it is NOT routed through
    /// `try`/`catch`; the flow fails cleanly.
    fn raise_malformed(&mut self, cause: String) -> Option<u64> {
        self.done = true;
        let flow = self
            .flow_name
            .clone()
            .unwrap_or_else(|| "<sol_vm>".to_string());
        self.last_error = Some(RemoteCallError::local(
            "<sol_vm>",
            flow,
            format!("malformed bytecode: {cause}"),
        ));
        Some(VM_ERROR_SENTINEL)
    }

    pub fn run(&mut self) -> u64 {
        loop {
            if let Some(result) = self.step() {
                return result;
            }
        }
    }

    pub fn step(&mut self) -> Option<u64> {
        if self.done {
            return None;
        }

        if self.inst_ptr >= self.program.len() {
            self.done = true;
            return Some(self.stack.pop().unwrap_or(0));
        }

        // P6: fuel check fires BEFORE the instruction runs so
        // an attacker cannot squeeze in one final side-effect
        // by stepping the counter to exactly 0. On exhaustion
        // we set last_error to a synthetic
        // RemoteCallError + the typed `fuel_exhausted_steps`
        // flag, log a WARN with flow name + step count, and
        // halt with VM_ERROR_SENTINEL.
        if self.fuel == 0 {
            self.done = true;
            self.fuel_exhausted_steps = Some(self.steps_taken);
            let flow = self.flow_name.as_deref().unwrap_or("<sol_vm>");
            // Index the last attempted instruction in the
            // log so operators see where the runaway flow
            // was when fuel hit zero.
            let last_inst_ptr = self.inst_ptr;
            let cause = format!(
                "sol VM fuel exhausted after {} steps (instruction index {last_inst_ptr})",
                self.steps_taken
            );
            tracing::warn!(
                flow = %flow,
                steps = self.steps_taken,
                last_instruction_index = last_inst_ptr,
                "sol VM fuel exhausted — flow exceeded its #steps / [sol] max_steps budget"
            );
            self.last_error = Some(RemoteCallError::local("<sol_vm>", flow, cause));
            return Some(VM_ERROR_SENTINEL);
        }
        self.fuel -= 1;
        self.steps_taken = self.steps_taken.saturating_add(1);

        let inst = self.program[self.inst_ptr].clone();
        self.inst_ptr += 1;
        match inst {
            // --- 1. Data Transport & Storage ---
            Inst::PushConst(ast_node) => {
                let bits = match ast_node {
                    Ast::ExprInteger(v) => v as u64,
                    Ast::ExprFloat(v) => v.to_bits(),
                    Ast::ExprChar(v) => v as u64,
                    Ast::ExprBool(v) => {
                        if v {
                            1
                        } else {
                            0
                        }
                    }
                    Ast::ExprUndefined => 0,
                    Ast::ExprString(s) => {
                        self.heap.push(HeapObject::String(s.clone()));
                        (self.heap.len() - 1) as u64
                    }
                    _ => {
                        self.fault("invalid constant AST node passed to PushConst");
                        0
                    }
                };
                self.push(bits);
            }

            Inst::LoadLocal(offset) => {
                let idx = (self.fp as isize + offset) as usize;
                match self.stack.get(idx) {
                    Some(&val) => self.push(val),
                    None => self.fault(format!(
                        "LoadLocal: slot {idx} out of range (stack len {})",
                        self.stack.len()
                    )),
                }
            }

            Inst::StoreLocal(offset) => {
                let val = self.pop();
                let idx = (self.fp as isize + offset) as usize;
                // A malformed offset (or a corrupted fp) can make
                // `idx` enormous; growing the stack to fill it
                // would OOM the worker. Bound the growth against
                // the allocation budget before zero-filling.
                let needed = idx.saturating_add(1);
                let grow_by = needed.saturating_sub(self.stack.len());
                if self.check_alloc(grow_by, "StoreLocal stack growth") {
                    while self.stack.len() < needed {
                        self.stack.push(0);
                    }
                    self.stack[idx] = val;
                }
            }

            Inst::Pop => {
                self.pop();
            }

            Inst::Dup => match self.stack.last().copied() {
                Some(val) => self.push(val),
                None => self.fault("Dup on empty stack"),
            },

            // --- 2. Integer Math & Comparisons ---
            Inst::IntAdd => {
                let b = self.pop() as i64;
                let a = self.pop() as i64;
                self.push((a + b) as u64);
            }
            Inst::IntSub => {
                let b = self.pop() as i64;
                let a = self.pop() as i64;
                self.push((a - b) as u64);
            }
            Inst::IntMul => {
                let b = self.pop() as i64;
                let a = self.pop() as i64;
                self.push((a * b) as u64);
            }
            Inst::IntDiv => {
                let b = self.pop() as i64;
                let a = self.pop() as i64;
                // `a / b` panics (aborting the worker) on divide
                // by zero and on `i64::MIN / -1` overflow. Both
                // are reachable from a flow, so convert them to a
                // structured fault.
                match a.checked_div(b) {
                    Some(q) => self.push(q as u64),
                    None => self.fault(format!("IntDiv: division error ({a} / {b})")),
                }
            }

            Inst::IntEq => {
                let b = self.pop() as i64;
                let a = self.pop() as i64;
                self.push(if a == b { 1 } else { 0 });
            }
            Inst::IntNeq => {
                let b = self.pop() as i64;
                let a = self.pop() as i64;
                self.push(if a != b { 1 } else { 0 });
            }
            Inst::IntGt => {
                let b = self.pop() as i64;
                let a = self.pop() as i64;
                self.push(if a > b { 1 } else { 0 });
            }
            Inst::IntGte => {
                let b = self.pop() as i64;
                let a = self.pop() as i64;
                self.push(if a >= b { 1 } else { 0 });
            }
            Inst::IntLt => {
                let b = self.pop() as i64;
                let a = self.pop() as i64;
                self.push(if a < b { 1 } else { 0 });
            }
            Inst::IntLte => {
                let b = self.pop() as i64;
                let a = self.pop() as i64;
                self.push(if a <= b { 1 } else { 0 });
            }

            // --- 3. Floating-Point Math & Comparisons ---
            Inst::FloatAdd => {
                let b = f64::from_bits(self.pop());
                let a = f64::from_bits(self.pop());
                self.push((a + b).to_bits());
            }
            Inst::FloatSub => {
                let b = f64::from_bits(self.pop());
                let a = f64::from_bits(self.pop());
                self.push((a - b).to_bits());
            }
            Inst::FloatMul => {
                let b = f64::from_bits(self.pop());
                let a = f64::from_bits(self.pop());
                self.push((a * b).to_bits());
            }
            Inst::FloatDiv => {
                let b = f64::from_bits(self.pop());
                let a = f64::from_bits(self.pop());
                self.push((a / b).to_bits());
            }

            Inst::FloatEq => {
                let b = f64::from_bits(self.pop());
                let a = f64::from_bits(self.pop());
                self.push(if a == b { 1 } else { 0 });
            }
            Inst::FloatNeq => {
                let b = f64::from_bits(self.pop());
                let a = f64::from_bits(self.pop());
                self.push(if a != b { 1 } else { 0 });
            }
            Inst::FloatGt => {
                let b = f64::from_bits(self.pop());
                let a = f64::from_bits(self.pop());
                self.push(if a > b { 1 } else { 0 });
            }
            Inst::FloatGte => {
                let b = f64::from_bits(self.pop());
                let a = f64::from_bits(self.pop());
                self.push(if a >= b { 1 } else { 0 });
            }
            Inst::FloatLt => {
                let b = f64::from_bits(self.pop());
                let a = f64::from_bits(self.pop());
                self.push(if a < b { 1 } else { 0 });
            }
            Inst::FloatLte => {
                let b = f64::from_bits(self.pop());
                let a = f64::from_bits(self.pop());
                self.push(if a <= b { 1 } else { 0 });
            }

            // --- 4. Char Comparisons ---
            Inst::CharEq => {
                let b = self.pop();
                let a = self.pop();
                self.push(if a == b { 1 } else { 0 });
            }
            Inst::CharNeq => {
                let b = self.pop();
                let a = self.pop();
                self.push(if a != b { 1 } else { 0 });
            }
            Inst::CharGt => {
                let b = self.pop();
                let a = self.pop();
                self.push(if a > b { 1 } else { 0 });
            }
            Inst::CharGte => {
                let b = self.pop();
                let a = self.pop();
                self.push(if a >= b { 1 } else { 0 });
            }
            Inst::CharLt => {
                let b = self.pop();
                let a = self.pop();
                self.push(if a < b { 1 } else { 0 });
            }
            Inst::CharLte => {
                let b = self.pop();
                let a = self.pop();
                self.push(if a <= b { 1 } else { 0 });
            }

            // --- 5. Logical & Bitwise ---
            Inst::LogOr => {
                let b = self.pop();
                let a = self.pop();
                self.push(if a == 1 || b == 1 { 1 } else { 0 });
            }
            Inst::LogAnd => {
                let b = self.pop();
                let a = self.pop();
                self.push(if a == 1 && b == 1 { 1 } else { 0 });
            }
            Inst::LogNot => {
                let a = self.pop();
                self.push(if a == 0 { 1 } else { 0 });
            }

            Inst::BitXor => {
                let b = self.pop();
                let a = self.pop();
                self.push(a ^ b);
            }
            Inst::BitAnd => {
                let b = self.pop();
                let a = self.pop();
                self.push(a & b);
            }
            Inst::BitOr => {
                let b = self.pop();
                let a = self.pop();
                self.push(a | b);
            }
            Inst::BitNeg => {
                let a = self.pop();
                self.push(!a);
            }
            Inst::BitLShift => {
                let b = self.pop();
                let a = self.pop();
                self.push(a << b);
            }
            Inst::BitRShift => {
                let b = self.pop();
                let a = self.pop();
                self.push(a >> b);
            }

            // --- 6. Compound Structures (Heap Interaction) ---
            Inst::NewStruct(fields) => {
                if self.check_alloc(fields, "NewStruct") {
                    let mut elements = vec![0; fields];
                    for i in (0..fields).rev() {
                        elements[i] = self.pop();
                    }
                    self.heap.push(HeapObject::Struct(elements));
                    self.push((self.heap.len() - 1) as u64);
                }
            }

            Inst::GetField(idx) => {
                let struct_ref = self.pop() as usize;
                match self.heap.get(struct_ref) {
                    Some(HeapObject::Struct(fields)) => match fields.get(idx) {
                        Some(&v) => self.push(v),
                        None => self.fault(format!("GetField: field index {idx} out of range")),
                    },
                    _ => self.fault(format!("GetField: heap ref {struct_ref} is not a struct")),
                }
            }

            Inst::SetField(idx) => {
                let struct_ref = self.pop() as usize;
                let value = self.pop();
                match self.heap.get_mut(struct_ref) {
                    Some(HeapObject::Struct(fields)) => match fields.get_mut(idx) {
                        Some(slot) => {
                            *slot = value;
                            self.push(value);
                        }
                        None => self.fault(format!("SetField: field index {idx} out of range")),
                    },
                    _ => self.fault(format!("SetField: heap ref {struct_ref} is not a struct")),
                }
            }

            Inst::NewArray => {
                let size = self.pop() as usize;
                // The size is operator-controlled (popped from the
                // stack); bound it against the allocation budget
                // so `size = usize::MAX` cannot OOM the worker.
                if self.check_alloc(size, "NewArray") {
                    self.heap.push(HeapObject::Array(vec![0; size]));
                    self.push((self.heap.len() - 1) as u64);
                }
            }

            Inst::ArrayLen => {
                let arr_ref = self.pop() as usize;
                match self.heap.get(arr_ref) {
                    Some(HeapObject::Array(items)) => {
                        let len = items.len() as u64;
                        self.push(len);
                    }
                    _ => self.fault(format!("ArrayLen: heap ref {arr_ref} is not an array")),
                }
            }

            Inst::GetElem => {
                let idx = self.pop() as usize;
                let arr_ref = self.pop() as usize;
                match self.heap.get(arr_ref) {
                    Some(HeapObject::Array(items)) => {
                        let len = items.len();
                        match items.get(idx).copied() {
                            Some(v) => self.push(v),
                            None => self
                                .fault(format!("GetElem: index {idx} out of bounds (len {len})")),
                        }
                    }
                    _ => self.fault(format!("GetElem: heap ref {arr_ref} is not an array")),
                }
            }

            Inst::SetElem => {
                let value = self.pop();
                let idx = self.pop() as usize;
                let arr_ref = self.pop() as usize;
                let fault = match self.heap.get_mut(arr_ref) {
                    Some(HeapObject::Array(items)) => {
                        let len = items.len();
                        match items.get_mut(idx) {
                            Some(slot) => {
                                *slot = value;
                                None
                            }
                            None => Some(format!("SetElem: index {idx} out of bounds (len {len})")),
                        }
                    }
                    _ => Some(format!("SetElem: heap ref {arr_ref} is not an array")),
                };
                match fault {
                    None => self.push(value),
                    Some(cause) => self.fault(cause),
                }
            }

            Inst::ConcatStr => {
                let idx2 = self.pop() as usize;
                let idx1 = self.pop() as usize;
                match (self.heap.get(idx1), self.heap.get(idx2)) {
                    (Some(HeapObject::String(s1)), Some(HeapObject::String(s2))) => {
                        let merged = format!("{}{}", s1, s2);
                        self.heap.push(HeapObject::String(merged));
                        self.push((self.heap.len() - 1) as u64);
                    }
                    _ => self.fault(format!(
                        "ConcatStr: heap refs {idx1}/{idx2} are not both strings"
                    )),
                }
            }

            Inst::EqStr => {
                let idx2 = self.pop() as usize;
                let idx1 = self.pop() as usize;
                match (self.heap.get(idx1), self.heap.get(idx2)) {
                    (Some(HeapObject::String(s1)), Some(HeapObject::String(s2))) => {
                        let eq = if s1 == s2 { 1 } else { 0 };
                        self.push(eq);
                    }
                    _ => self.fault(format!(
                        "EqStr: heap refs {idx1}/{idx2} are not both strings"
                    )),
                }
            }

            // --- 7. Control Flow & Jumps ---
            Inst::Jump(target) => {
                self.inst_ptr = target;
            }

            Inst::JumpFalse(target) => {
                if self.pop() == 0 {
                    self.inst_ptr = target;
                }
            }

            Inst::Call(target, arg_count) => {
                // `stack.len() - arg_count` underflows (usize wrap
                // → bogus fp → later panic) when malformed
                // bytecode claims more arguments than are on the
                // stack. Check it.
                match self.stack.len().checked_sub(arg_count) {
                    Some(new_fp) => {
                        self.call_stack.push(Frame {
                            return_ptr: self.inst_ptr,
                            old_fp: self.fp,
                        });
                        self.fp = new_fp;
                        self.inst_ptr = target;
                    }
                    None => self.fault(format!(
                        "Call: arg_count {arg_count} exceeds stack depth {}",
                        self.stack.len()
                    )),
                }
            }

            Inst::Ret => {
                if let Some(frame) = self.call_stack.pop() {
                    self.stack.truncate(self.fp);
                    self.fp = frame.old_fp;
                    self.inst_ptr = frame.return_ptr;
                    self.push(0);
                } else {
                    self.done = true;
                    let v = self.pop();
                    if let Some(cause) = self.fault.take() {
                        return self.raise_malformed(cause);
                    }
                    return Some(v);
                }
            }

            Inst::RetVal => {
                let return_value = self.pop();
                if let Some(frame) = self.call_stack.pop() {
                    self.stack.truncate(self.fp);
                    self.fp = frame.old_fp;
                    self.inst_ptr = frame.return_ptr;
                    self.push(return_value);
                } else {
                    self.done = true;
                    if let Some(cause) = self.fault.take() {
                        return self.raise_malformed(cause);
                    }
                    return Some(return_value);
                }
            }

            // --- 8. System Explicit Outputs (Yields Void/0 to align stack execution pipelines) ---
            Inst::PrintInt => {
                println!("{}", self.pop() as i64);
                let _ = io::stdout().flush();
                self.push(0);
            }

            Inst::PrintFloat => {
                println!("{}", f64::from_bits(self.pop()));
                let _ = io::stdout().flush();
                self.push(0);
            }

            Inst::PrintChar => {
                if let Some(c) = char::from_u32(self.pop() as u32) {
                    println!("{}", c);
                }
                let _ = io::stdout().flush();
                self.push(0);
            }

            Inst::PrintString => {
                let idx = self.pop() as usize;
                match self.heap.get(idx) {
                    Some(HeapObject::String(s)) => println!("{}", s),
                    _ => self.fault(format!("PrintString: heap ref {idx} is not a string")),
                }
                let _ = io::stdout().flush();
                self.push(0);
            }

            // ---- Relix extensions (M6) ----
            //
            // RemoteCall pops three heap-string refs (arg, method, peer in
            // pop-order — i.e. peer was pushed first, arg last), invokes the
            // attached dispatcher, and pushes the response as a fresh
            // HeapObject::String. On any failure the VM halts with
            // VM_ERROR_SENTINEL and `last_error()` carries the cause.
            Inst::RemoteCall => {
                // Pop in reverse-push order.
                let arg_ref = self.pop() as usize;
                let method_ref = self.pop() as usize;
                let peer_ref = self.pop() as usize;

                let arg_str = match self.heap.get(arg_ref) {
                    Some(HeapObject::String(s)) => s.clone(),
                    _ => {
                        self.last_error = Some(RemoteCallError::local(
                            "<unresolved>",
                            "<unresolved>",
                            "remote_call: arg is not a heap string",
                        ));
                        self.done = true;
                        return Some(VM_ERROR_SENTINEL);
                    }
                };
                let method_str = match self.heap.get(method_ref) {
                    Some(HeapObject::String(s)) => s.clone(),
                    _ => {
                        self.last_error = Some(RemoteCallError::local(
                            "<unresolved>",
                            "<unresolved>",
                            "remote_call: method is not a heap string",
                        ));
                        self.done = true;
                        return Some(VM_ERROR_SENTINEL);
                    }
                };
                let peer_str = match self.heap.get(peer_ref) {
                    Some(HeapObject::String(s)) => s.clone(),
                    _ => {
                        self.last_error = Some(RemoteCallError::local(
                            "<unresolved>",
                            method_str.clone(),
                            "remote_call: peer is not a heap string",
                        ));
                        self.done = true;
                        return Some(VM_ERROR_SENTINEL);
                    }
                };

                let Some(dispatcher) = self.dispatcher.clone() else {
                    self.last_error = Some(RemoteCallError::local(
                        peer_str,
                        method_str,
                        "no RemoteCallDispatcher attached to VM",
                    ));
                    self.done = true;
                    return Some(VM_ERROR_SENTINEL);
                };

                match dispatcher.remote_call(&peer_str, &method_str, arg_str.as_bytes()) {
                    Ok(body) => {
                        let response = String::from_utf8(body).unwrap_or_else(|e| {
                            format!("<binary response: {} bytes; {}>", e.as_bytes().len(), e)
                        });
                        self.heap.push(HeapObject::String(response));
                        self.push((self.heap.len() - 1) as u64);
                        self.last_error = None;
                    }
                    Err(e) => {
                        self.last_error = Some(e);
                        if let Some(sentinel) = self.try_dispatch_error() {
                            return Some(sentinel);
                        }
                    }
                }
            }

            // RELIX-2 step 4: streaming variant of RemoteCall.
            // Same stack contract — pops arg / method / peer
            // in reverse-push order, pushes a heap-string ref
            // to the concatenated response body. The
            // difference vs `Inst::RemoteCall` is which
            // dispatcher method is invoked
            // (`remote_call_stream`) and the optional
            // chunk-observer callback wired by the host.
            // Each chunk fires the observer in arrival order
            // BEFORE the VM has finished collecting — the
            // web bridge uses this to ship tokens to SSE
            // while the SOL flow is still running.
            Inst::RemoteCallStream => {
                let arg_ref = self.pop() as usize;
                let method_ref = self.pop() as usize;
                let peer_ref = self.pop() as usize;

                let arg_str = match self.heap.get(arg_ref) {
                    Some(HeapObject::String(s)) => s.clone(),
                    _ => {
                        self.last_error = Some(RemoteCallError::local(
                            "<unresolved>",
                            "<unresolved>",
                            "remote_call_stream: arg is not a heap string",
                        ));
                        self.done = true;
                        return Some(VM_ERROR_SENTINEL);
                    }
                };
                let method_str = match self.heap.get(method_ref) {
                    Some(HeapObject::String(s)) => s.clone(),
                    _ => {
                        self.last_error = Some(RemoteCallError::local(
                            "<unresolved>",
                            "<unresolved>",
                            "remote_call_stream: method is not a heap string",
                        ));
                        self.done = true;
                        return Some(VM_ERROR_SENTINEL);
                    }
                };
                let peer_str = match self.heap.get(peer_ref) {
                    Some(HeapObject::String(s)) => s.clone(),
                    _ => {
                        self.last_error = Some(RemoteCallError::local(
                            "<unresolved>",
                            method_str.clone(),
                            "remote_call_stream: peer is not a heap string",
                        ));
                        self.done = true;
                        return Some(VM_ERROR_SENTINEL);
                    }
                };

                let Some(dispatcher) = self.dispatcher.clone() else {
                    self.last_error = Some(RemoteCallError::local(
                        peer_str,
                        method_str,
                        "no RemoteCallDispatcher attached to VM",
                    ));
                    self.done = true;
                    return Some(VM_ERROR_SENTINEL);
                };

                // Pass the chunk observer to the dispatcher.
                // When unset, supply a no-op so the dispatcher's
                // default impl can still call it once.
                let observer = self.chunk_observer.clone();
                let noop: Arc<dyn Fn(&[u8]) + Send + Sync> = Arc::new(|_| {});
                let on_chunk_arc = observer.unwrap_or(noop);
                let on_chunk = |bytes: &[u8]| on_chunk_arc(bytes);

                match dispatcher.remote_call_stream(
                    &peer_str,
                    &method_str,
                    arg_str.as_bytes(),
                    &on_chunk,
                ) {
                    Ok(body) => {
                        let response = String::from_utf8(body).unwrap_or_else(|e| {
                            format!("<binary response: {} bytes; {}>", e.as_bytes().len(), e)
                        });
                        self.heap.push(HeapObject::String(response));
                        self.push((self.heap.len() - 1) as u64);
                        self.last_error = None;
                    }
                    Err(e) => {
                        self.last_error = Some(e);
                        if let Some(sentinel) = self.try_dispatch_error() {
                            return Some(sentinel);
                        }
                    }
                }
            }

            // --- F2: try / catch / rethrow ---
            Inst::TryEnter(catch_pc) => {
                self.try_handlers.push(TryHandler {
                    catch_pc,
                    fp_at_enter: self.fp,
                    stack_len_at_enter: self.stack.len(),
                });
            }
            Inst::TryExit => {
                // Successful exit from the try body — pop the
                // handler so an enclosing try doesn't see a
                // stale entry.
                self.try_handlers.pop();
            }
            Inst::LoadErrorKind => {
                let kind = self
                    .last_error
                    .as_ref()
                    .map(classified_error_kind)
                    .unwrap_or("");
                self.heap.push(HeapObject::String(kind.to_string()));
                self.push((self.heap.len() - 1) as u64);
            }
            Inst::LoadErrorCause => {
                let cause = self
                    .last_error
                    .as_ref()
                    .map(|e| e.cause.clone())
                    .unwrap_or_default();
                self.heap.push(HeapObject::String(cause));
                self.push((self.heap.len() - 1) as u64);
            }
            Inst::LoadErrorRetryHint => {
                // Not currently carried on RemoteCallError —
                // surface 0 so flows that read it inside a
                // catch get a deterministic value rather than a
                // panic. Future commits can add the field to
                // the dispatcher trait.
                self.push(0);
            }
            Inst::LoadLastConfidence => {
                // RELIX-7.19: zero-arg builtin. Push the f32
                // confidence (widened to f64) as the bit
                // pattern the SOL float opcodes expect.
                let v = if let Some(cell) = &self.last_confidence_cell {
                    cell.get()
                } else {
                    self.last_confidence
                };
                self.push((v as f64).to_bits());
            }
            Inst::Rethrow => {
                if let Some(sentinel) = self.try_dispatch_error() {
                    return Some(sentinel);
                }
            }

            // ---- F5 / F7: list & map opcodes ----
            Inst::PushList(n) => {
                if self.check_alloc(n, "PushList") {
                    let mut elements: Vec<u64> = vec![0; n];
                    for slot in elements.iter_mut().rev() {
                        *slot = self.pop();
                    }
                    self.heap.push(HeapObject::List(elements));
                    self.push((self.heap.len() - 1) as u64);
                }
            }
            Inst::PushMap(n) => {
                // Stack layout: ..., key1, val1, key2, val2, ..., keyN, valN
                // (alternating, with valN on top.) Pop into a
                // temporary vec then reverse so insertion order
                // matches source order.
                if !self.check_alloc(n, "PushMap") {
                    // Oversized operand — bail before reserving.
                } else {
                    let mut pairs: Vec<(String, u64)> = Vec::with_capacity(n);
                    for _ in 0..n {
                        let value = self.pop();
                        let key_ref = self.pop() as usize;
                        let key = match self.heap.get(key_ref) {
                            Some(HeapObject::String(s)) => s.clone(),
                            _ => String::new(),
                        };
                        pairs.push((key, value));
                    }
                    pairs.reverse();
                    self.heap.push(HeapObject::Map(pairs));
                    self.push((self.heap.len() - 1) as u64);
                }
            }
            Inst::ListLen => {
                let lst_ref = self.pop() as usize;
                let len = match self.heap.get(lst_ref) {
                    Some(HeapObject::List(items)) => items.len(),
                    Some(HeapObject::Array(items)) => items.len(),
                    _ => 0,
                };
                self.push(len as u64);
            }
            Inst::ListGet => {
                let idx_raw = self.pop() as i64;
                let lst_ref = self.pop() as usize;
                // Resolve to a string from the heap; out of
                // bounds / wrong-type / non-string element all
                // return the empty string (push a fresh heap
                // string so subsequent ops see a real ref).
                let result_idx: u64 = match self.heap.get(lst_ref) {
                    Some(HeapObject::List(items)) => {
                        if idx_raw < 0 || (idx_raw as usize) >= items.len() {
                            self.heap.push(HeapObject::String(String::new()));
                            (self.heap.len() - 1) as u64
                        } else {
                            items[idx_raw as usize]
                        }
                    }
                    Some(HeapObject::Array(items)) => {
                        if idx_raw < 0 || (idx_raw as usize) >= items.len() {
                            self.heap.push(HeapObject::String(String::new()));
                            (self.heap.len() - 1) as u64
                        } else {
                            items[idx_raw as usize]
                        }
                    }
                    _ => {
                        self.heap.push(HeapObject::String(String::new()));
                        (self.heap.len() - 1) as u64
                    }
                };
                self.push(result_idx);
            }
            Inst::ListPush => {
                let val = self.pop();
                let lst_ref = self.pop() as usize;
                let mut new_items: Vec<u64> = match self.heap.get(lst_ref) {
                    Some(HeapObject::List(items)) => items.clone(),
                    Some(HeapObject::Array(items)) => items.clone(),
                    _ => Vec::new(),
                };
                new_items.push(val);
                self.heap.push(HeapObject::List(new_items));
                self.push((self.heap.len() - 1) as u64);
            }
            Inst::ListContains => {
                let val_ref = self.pop() as usize;
                let lst_ref = self.pop() as usize;
                let needle = heap_display(&self.heap, val_ref as u64);
                let items: Vec<u64> = match self.heap.get(lst_ref) {
                    Some(HeapObject::List(items)) => items.clone(),
                    Some(HeapObject::Array(items)) => items.clone(),
                    _ => Vec::new(),
                };
                // F11: needle comparison goes through
                // `heap_display` so a string can be matched
                // against a nested list element that
                // stringifies to the same value.
                let found = items.iter().any(|r| heap_display(&self.heap, *r) == needle);
                self.push(if found { 1 } else { 0 });
            }
            Inst::ListJoin => {
                let sep_ref = self.pop() as usize;
                let lst_ref = self.pop() as usize;
                let sep = match self.heap.get(sep_ref) {
                    Some(HeapObject::String(s)) => s.clone(),
                    _ => String::new(),
                };
                let items: Vec<u64> = match self.heap.get(lst_ref) {
                    Some(HeapObject::List(items)) => items.clone(),
                    Some(HeapObject::Array(items)) => items.clone(),
                    _ => Vec::new(),
                };
                // F11: nested list / map elements stringify
                // recursively. A list-of-lists [[a, b], [c, d]]
                // joined with `,` yields `a|b,c|d` — the inner
                // list uses the canonical pipe separator,
                // matching Sflow's `SflowValue::to_display`.
                let parts: Vec<String> = items
                    .into_iter()
                    .map(|r| heap_display(&self.heap, r))
                    .collect();
                let joined = parts.join(&sep);
                self.heap.push(HeapObject::String(joined));
                self.push((self.heap.len() - 1) as u64);
            }
            Inst::ListGetList => {
                let idx_raw = self.pop() as i64;
                let lst_ref = self.pop() as usize;
                let element_ref: Option<u64> = match self.heap.get(lst_ref) {
                    Some(HeapObject::List(items)) => {
                        if idx_raw < 0 || (idx_raw as usize) >= items.len() {
                            None
                        } else {
                            Some(items[idx_raw as usize])
                        }
                    }
                    _ => None,
                };
                let Some(elem) = element_ref else {
                    self.last_error = Some(RemoteCallError::local(
                        "<local>",
                        "list_get_list",
                        format!("list_get_list: index {idx_raw} out of bounds or not a list"),
                    ));
                    if let Some(sentinel) = self.try_dispatch_error() {
                        return Some(sentinel);
                    } else {
                        // Re-route through error dispatch — if
                        // there's no handler the VM already
                        // halted; this branch is only reached
                        // when a handler caught the error.
                        return None;
                    }
                };
                // Element must be a list.
                match self.heap.get(elem as usize) {
                    Some(HeapObject::List(_)) => {
                        self.push(elem);
                    }
                    _ => {
                        self.last_error = Some(RemoteCallError::local(
                            "<local>",
                            "list_get_list",
                            format!("list_get_list: element at {idx_raw} is not a list"),
                        ));
                        if let Some(sentinel) = self.try_dispatch_error() {
                            return Some(sentinel);
                        } else {
                            return None;
                        }
                    }
                }
            }
            Inst::MapGetMap => {
                let key_ref = self.pop() as usize;
                let map_ref = self.pop() as usize;
                let key = match self.heap.get(key_ref) {
                    Some(HeapObject::String(s)) => s.clone(),
                    _ => String::new(),
                };
                let val_ref: Option<u64> = match self.heap.get(map_ref) {
                    Some(HeapObject::Map(pairs)) => {
                        pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
                    }
                    _ => None,
                };
                let Some(val) = val_ref else {
                    self.last_error = Some(RemoteCallError::local(
                        "<local>",
                        "map_get_map",
                        format!("map_get_map: key `{key}` not present"),
                    ));
                    if let Some(sentinel) = self.try_dispatch_error() {
                        return Some(sentinel);
                    } else {
                        return None;
                    }
                };
                match self.heap.get(val as usize) {
                    Some(HeapObject::Map(_)) => {
                        self.push(val);
                    }
                    _ => {
                        self.last_error = Some(RemoteCallError::local(
                            "<local>",
                            "map_get_map",
                            format!("map_get_map: value at `{key}` is not a map"),
                        ));
                        if let Some(sentinel) = self.try_dispatch_error() {
                            return Some(sentinel);
                        } else {
                            return None;
                        }
                    }
                }
            }
            Inst::ListSplit => {
                let sep_ref = self.pop() as usize;
                let str_ref = self.pop() as usize;
                let s = match self.heap.get(str_ref) {
                    Some(HeapObject::String(s)) => s.clone(),
                    _ => String::new(),
                };
                let sep = match self.heap.get(sep_ref) {
                    Some(HeapObject::String(s)) => s.clone(),
                    _ => String::new(),
                };
                // Empty source splits to a single empty
                // element (matches Rust's str::split). The task
                // spec says "empty string produces
                // single-element list" which agrees.
                let parts: Vec<&str> = if sep.is_empty() {
                    // Avoid splitting on an empty separator
                    // (that gives an unbounded iterator). Yield
                    // the whole string as a single element.
                    vec![s.as_str()]
                } else {
                    s.split(sep.as_str()).collect()
                };
                let mut refs: Vec<u64> = Vec::with_capacity(parts.len());
                for p in parts {
                    self.heap.push(HeapObject::String(p.to_string()));
                    refs.push((self.heap.len() - 1) as u64);
                }
                self.heap.push(HeapObject::List(refs));
                self.push((self.heap.len() - 1) as u64);
            }
            Inst::MapGet => {
                let key_ref = self.pop() as usize;
                let map_ref = self.pop() as usize;
                let key = match self.heap.get(key_ref) {
                    Some(HeapObject::String(s)) => s.clone(),
                    _ => String::new(),
                };
                let val_ref: u64 = match self.heap.get(map_ref) {
                    Some(HeapObject::Map(pairs)) => pairs
                        .iter()
                        .find(|(k, _)| *k == key)
                        .map(|(_, v)| *v)
                        .unwrap_or_else(|| {
                            self.heap.push(HeapObject::String(String::new()));
                            (self.heap.len() - 1) as u64
                        }),
                    _ => {
                        self.heap.push(HeapObject::String(String::new()));
                        (self.heap.len() - 1) as u64
                    }
                };
                // If the looked-up value was found and is not a
                // heap string, return its raw ref. Callers
                // typically use `map_get` to read string values
                // but the VM doesn't enforce that.
                self.push(val_ref);
            }
            Inst::MapSet => {
                let val = self.pop();
                let key_ref = self.pop() as usize;
                let map_ref = self.pop() as usize;
                let key = match self.heap.get(key_ref) {
                    Some(HeapObject::String(s)) => s.clone(),
                    _ => String::new(),
                };
                let mut pairs: Vec<(String, u64)> = match self.heap.get(map_ref) {
                    Some(HeapObject::Map(p)) => p.clone(),
                    _ => Vec::new(),
                };
                if let Some(existing) = pairs.iter_mut().find(|(k, _)| *k == key) {
                    existing.1 = val;
                } else {
                    pairs.push((key, val));
                }
                self.heap.push(HeapObject::Map(pairs));
                self.push((self.heap.len() - 1) as u64);
            }
            Inst::MapHas => {
                let key_ref = self.pop() as usize;
                let map_ref = self.pop() as usize;
                let key = match self.heap.get(key_ref) {
                    Some(HeapObject::String(s)) => s.clone(),
                    _ => String::new(),
                };
                let has = matches!(
                    self.heap.get(map_ref),
                    Some(HeapObject::Map(pairs)) if pairs.iter().any(|(k, _)| *k == key)
                );
                self.push(if has { 1 } else { 0 });
            }
            Inst::MapKeys => {
                let map_ref = self.pop() as usize;
                let keys: Vec<String> = match self.heap.get(map_ref) {
                    Some(HeapObject::Map(pairs)) => pairs.iter().map(|(k, _)| k.clone()).collect(),
                    _ => Vec::new(),
                };
                let mut refs: Vec<u64> = Vec::with_capacity(keys.len());
                for k in keys {
                    self.heap.push(HeapObject::String(k));
                    refs.push((self.heap.len() - 1) as u64);
                }
                self.heap.push(HeapObject::List(refs));
                self.push((self.heap.len() - 1) as u64);
            }
            Inst::MapLen => {
                let map_ref = self.pop() as usize;
                let len = match self.heap.get(map_ref) {
                    Some(HeapObject::Map(pairs)) => pairs.len(),
                    _ => 0,
                };
                self.push(len as u64);
            }
            Inst::MapDel => {
                let key_ref = self.pop() as usize;
                let map_ref = self.pop() as usize;
                let key = match self.heap.get(key_ref) {
                    Some(HeapObject::String(s)) => s.clone(),
                    _ => String::new(),
                };
                let pairs: Vec<(String, u64)> = match self.heap.get(map_ref) {
                    Some(HeapObject::Map(p)) => {
                        p.iter().filter(|(k, _)| *k != key).cloned().collect()
                    }
                    _ => Vec::new(),
                };
                self.heap.push(HeapObject::Map(pairs));
                self.push((self.heap.len() - 1) as u64);
            }
        }

        // Malformed-bytecode faults recorded by `pop()` or an
        // opcode handler during this instruction halt the VM
        // cleanly with a structured `last_error` rather than
        // aborting the worker.
        if let Some(cause) = self.fault.take() {
            return self.raise_malformed(cause);
        }

        None
    }

    /// F2: route the current `last_error` to the nearest
    /// active try-handler. Pops the handler, restores fp +
    /// stack length, jumps to the catch dispatch block.
    /// Returns `Some(VM_ERROR_SENTINEL)` when no handler is
    /// available — the caller bails out exactly the way the
    /// pre-F2 RemoteCall-failure path did.
    fn try_dispatch_error(&mut self) -> Option<u64> {
        let Some(handler) = self.try_handlers.pop() else {
            self.done = true;
            return Some(VM_ERROR_SENTINEL);
        };
        self.fp = handler.fp_at_enter;
        self.stack.truncate(handler.stack_len_at_enter);
        self.inst_ptr = handler.catch_pc;
        None
    }
}

/// F11: recursively stringify a heap ref. Strings yield their
/// body verbatim. Lists join their elements with `|` (recursing
/// through nested heap refs). Maps join `k=v` pairs with `;`
/// (recursing the values). Anything else (Struct, Array) yields
/// an empty string — those types don't have a documented
/// display format and operators shouldn't be relying on it
/// silently working.
fn heap_display(heap: &[HeapObject], idx: u64) -> String {
    match heap.get(idx as usize) {
        Some(HeapObject::String(s)) => s.clone(),
        Some(HeapObject::List(items)) => items
            .iter()
            .map(|r| heap_display(heap, *r))
            .collect::<Vec<_>>()
            .join("|"),
        Some(HeapObject::Map(pairs)) => pairs
            .iter()
            .map(|(k, v)| format!("{k}={}", heap_display(heap, *v)))
            .collect::<Vec<_>>()
            .join(";"),
        _ => String::new(),
    }
}

/// Classify a `RemoteCallError` into one of the catch-kind
/// labels SOL recognises. Mirrors Sflow's classification
/// (`sflow::executor::classify_remote_error`) so the two
/// languages agree on which errors land in which clause.
fn classified_error_kind(err: &RemoteCallError) -> &'static str {
    use relix_core::types::error_kinds;
    match err.kind {
        error_kinds::TIMEOUT | error_kinds::APPROVAL_TIMEOUT => "timeout",
        error_kinds::TRANSPORT | error_kinds::PEER_UNREACHABLE | 0 => "mesh_error",
        error_kinds::POLICY_DENIED
        | error_kinds::APPROVAL_DENIED
        | error_kinds::APPROVAL_REQUIRED => "policy_denied",
        _ => "responder_error",
    }
}

#[cfg(test)]
mod fuel_tests {
    //! P6 — fuel-counter tests for the VM. These exercise the
    //! VM directly (no compile / dispatch dance) so the
    //! assertions are tight and fast.

    use super::*;
    use crate::sol::bytecode::Inst;
    use crate::sol::{DEFAULT_MAX_STEPS, MAX_STEPS_CEILING};

    /// Build a VM with bytecode that loops forever via
    /// `Jump(0)`. Used by every fuel-exhaustion test below.
    fn loop_forever_vm() -> VM {
        // Jump(0) → Jump(0) → … One instruction per step;
        // the VM never hits the program-end branch.
        VM::from(&[Inst::Jump(0)])
    }

    #[test]
    fn p6_default_fuel_matches_default_max_steps() {
        let vm = VM::new();
        assert_eq!(vm.fuel(), DEFAULT_MAX_STEPS);
        assert_eq!(vm.steps_taken(), 0);
        assert!(vm.fuel_exhausted_steps().is_none());
    }

    #[test]
    fn p6_with_fuel_clamps_to_hard_ceiling() {
        let vm = VM::new().with_fuel(MAX_STEPS_CEILING * 2);
        assert_eq!(vm.fuel(), MAX_STEPS_CEILING);
    }

    #[test]
    fn p6_with_fuel_zero_clamps_to_one() {
        // A zero budget = instant exhaustion. We rewrite to 1
        // so the VM still has a single instruction to attempt
        // (matching the operator's likely intent if they
        // somehow pass 0).
        let vm = VM::new().with_fuel(0);
        assert_eq!(vm.fuel(), 1);
    }

    #[test]
    fn p6_loop_with_max_steps_50_returns_fuel_exhausted_after_50_steps() {
        // P6 test: "A SOL flow that loops 100 times with
        // max_steps = 50 returns FuelExhausted after 50 steps."
        let mut vm = loop_forever_vm().with_fuel(50);
        let exit = vm.run();
        assert_eq!(exit, VM_ERROR_SENTINEL);
        assert_eq!(vm.steps_taken(), 50);
        assert_eq!(vm.fuel_exhausted_steps(), Some(50));
        let err = vm
            .last_error()
            .expect("fuel exhaustion must set last_error");
        assert!(
            err.cause.contains("fuel exhausted"),
            "expected fuel-exhaustion cause, got: {}",
            err.cause
        );
        assert!(
            err.cause.contains("50 steps"),
            "cause must report step count: {}",
            err.cause
        );
    }

    #[test]
    fn p6_terminating_program_within_fuel_completes_successfully() {
        // P6 test: "A SOL flow that terminates normally
        // within the fuel limit completes successfully." We
        // exercise this via a program that produces no
        // instructions — the VM hits the program-end branch
        // and returns the (empty) stack top on its first
        // step. Fuel is not exhausted.
        let mut vm = VM::from(&[]).with_fuel(100);
        let exit = vm.run();
        // Empty program → step() takes the "ip past program"
        // branch and returns 0. No fuel was consumed; that's
        // fine — the contract is "no FuelExhausted as long as
        // the VM terminates inside the budget".
        assert_eq!(exit, 0);
        assert!(vm.fuel_exhausted_steps().is_none());
    }

    #[test]
    fn p6_fuel_exhaustion_logs_flow_name_when_provided() {
        // P6 test: "FuelExhausted logs the flow name and step
        // count." We assert the surface contract via
        // last_error rather than capturing the tracing event
        // — the cause string includes both pieces of info.
        let mut vm = loop_forever_vm()
            .with_fuel(10)
            .with_flow_name("dangerous_flow.sol");
        let exit = vm.run();
        assert_eq!(exit, VM_ERROR_SENTINEL);
        let err = vm.last_error().expect("fuel exhaustion sets last_error");
        // The flow name lands in the `method` field of the
        // synthetic RemoteCallError so log lines that scrape
        // `last_error()` see "dangerous_flow.sol".
        assert_eq!(err.method, "dangerous_flow.sol");
        assert_eq!(err.peer, "<sol_vm>");
        assert!(
            err.cause.contains("fuel exhausted after 10 steps"),
            "cause: {}",
            err.cause
        );
    }

    #[test]
    fn p6_subsequent_step_calls_after_exhaustion_return_none() {
        let mut vm = loop_forever_vm().with_fuel(3);
        let exit = vm.run();
        assert_eq!(exit, VM_ERROR_SENTINEL);
        // VM is done — further step() calls return None
        // without re-firing the fuel check.
        assert!(vm.step().is_none());
    }
}

#[cfg(test)]
mod malformed_bytecode_tests {
    //! Malformed / hostile bytecode must fail the flow cleanly
    //! (structured `last_error` + `VM_ERROR_SENTINEL`) rather
    //! than panicking the `spawn_blocking` worker. Bytecode
    //! reaching the VM is untrusted (it is compiled from
    //! operator/agent-authored SOL or YAML), so the VM treats
    //! every stack/heap/index/alloc invariant as attacker-
    //! controlled.

    use super::*;
    use crate::sol::bytecode::Inst;
    use crate::sol::parser::Ast;

    /// CRITERION 1 — a hand-crafted malformed program (stack
    /// underflow + a bad PushConst payload + an out-of-range
    /// local load) returns a structured error, not a panic.
    #[test]
    fn malformed_bytecode_returns_structured_error_not_panic() {
        // (a) Stack underflow: `Pop` with an empty stack.
        let mut vm = VM::from(&[Inst::Pop]);
        let exit = vm.run();
        assert_eq!(
            exit, VM_ERROR_SENTINEL,
            "stack underflow must halt with the error sentinel"
        );
        let err = vm
            .last_error()
            .expect("stack underflow must set a structured last_error");
        assert!(
            err.cause.contains("malformed bytecode") && err.cause.contains("stack underflow"),
            "unexpected cause: {}",
            err.cause
        );

        // (b) Bad PushConst: a non-constant AST node is an
        // invalid constant payload (only the compiler should
        // ever emit constant nodes here).
        let mut vm = VM::from(&[Inst::PushConst(Ast::ExprVar("x".to_string()))]);
        let exit = vm.run();
        assert_eq!(exit, VM_ERROR_SENTINEL);
        assert!(
            vm.last_error()
                .map(|e| e.cause.contains("invalid constant AST node"))
                .unwrap_or(false),
            "bad PushConst payload must surface a structured cause"
        );

        // (c) Out-of-range local slot (a "bad index"): load a
        // local far past the top of the stack.
        let mut vm = VM::from(&[Inst::LoadLocal(9999)]);
        let exit = vm.run();
        assert_eq!(exit, VM_ERROR_SENTINEL);
        assert!(
            vm.last_error()
                .map(|e| e.cause.contains("LoadLocal"))
                .unwrap_or(false),
            "out-of-range LoadLocal must surface a structured cause"
        );
    }

    /// CRITERION 2 — `NewArray` with `size = usize::MAX` returns
    /// a structured error instead of attempting a >16-exabyte
    /// allocation that would OOM the worker.
    #[test]
    fn newarray_with_max_size_returns_error_not_oom() {
        // PushConst(-1) → bits = (-1i128 as u64) = u64::MAX,
        // which NewArray reads as `usize::MAX` on 64-bit.
        let program = [Inst::PushConst(Ast::ExprInteger(-1)), Inst::NewArray];
        let mut vm = VM::from(&program);
        let exit = vm.run();
        assert_eq!(
            exit, VM_ERROR_SENTINEL,
            "an oversized NewArray must halt with the error sentinel"
        );
        let err = vm
            .last_error()
            .expect("oversized NewArray must set a structured last_error");
        assert!(
            err.cause.contains("NewArray") && err.cause.contains("allocation budget"),
            "unexpected cause: {}",
            err.cause
        );
    }

    /// Divide-by-zero — reachable from a flow — must fault
    /// cleanly rather than panicking the worker.
    #[test]
    fn int_div_by_zero_returns_error_not_panic() {
        let program = [
            Inst::PushConst(Ast::ExprInteger(7)),
            Inst::PushConst(Ast::ExprInteger(0)),
            Inst::IntDiv,
        ];
        let mut vm = VM::from(&program);
        let exit = vm.run();
        assert_eq!(exit, VM_ERROR_SENTINEL);
        assert!(
            vm.last_error()
                .map(|e| e.cause.contains("IntDiv"))
                .unwrap_or(false),
            "division by zero must surface a structured cause"
        );
    }

    /// A malformed `NewArray` mid-program must not corrupt the
    /// VM into a panic on a later instruction either — the VM
    /// halts immediately on the fault.
    #[test]
    fn fault_halts_immediately_and_is_not_catchable_as_success() {
        // GetElem with an empty stack: underflow on both pops,
        // then a heap lookup of ref 0 on an empty heap. Must
        // fault, not panic.
        let mut vm = VM::from(&[Inst::GetElem]);
        let exit = vm.run();
        assert_eq!(exit, VM_ERROR_SENTINEL);
        assert!(vm.last_error().is_some());
    }
}
