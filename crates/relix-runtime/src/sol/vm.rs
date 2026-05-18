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
}

struct Frame {
    return_ptr: usize,
    old_fp: usize,
}

pub struct VM {
    stack: Vec<u64>,
    heap: Vec<HeapObject>,
    call_stack: Vec<Frame>,
    inst_ptr: usize,
    fp: usize,
    program: Vec<Inst>,
    done: bool,
    /// Relix extension (M6): optional host-side dispatcher for `Inst::RemoteCall`.
    /// `None` means remote calls are forbidden — encountering `RemoteCall` halts
    /// the VM with a `local_dispatch_error`.
    dispatcher: Option<Arc<dyn RemoteCallDispatcher>>,
    /// Relix extension (M6): structured error from the last failed
    /// `RemoteCall`, if any. Cleared on successful step.
    last_error: Option<RemoteCallError>,
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
        }
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

    #[inline]
    fn pop(&mut self) -> u64 {
        self.stack.pop().expect("Runtime Error: Stack underflow")
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
                    _ => panic!("Runtime Error: Invalid constant AST node passed to VM"),
                };
                self.push(bits);
            }

            Inst::LoadLocal(offset) => {
                let idx = (self.fp as isize + offset) as usize;
                let val = self.stack[idx];
                self.push(val);
            }

            Inst::StoreLocal(offset) => {
                let val = self.pop();
                let idx = (self.fp as isize + offset) as usize;
                while self.stack.len() <= idx {
                    self.stack.push(0);
                }
                self.stack[idx] = val;
            }

            Inst::Pop => {
                self.pop();
            }

            Inst::Dup => {
                let val = *self
                    .stack
                    .last()
                    .expect("Runtime Error: Cannot DUP empty stack");
                self.push(val);
            }

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
                self.push((a / b) as u64);
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
                let mut elements = vec![0; fields];
                for i in (0..fields).rev() {
                    elements[i] = self.pop();
                }
                self.heap.push(HeapObject::Struct(elements));
                self.push((self.heap.len() - 1) as u64);
            }

            Inst::GetField(idx) => {
                let struct_ref = self.pop() as usize;
                if let HeapObject::Struct(fields) = &self.heap[struct_ref] {
                    self.push(fields[idx]);
                }
            }

            Inst::SetField(idx) => {
                let struct_ref = self.pop() as usize;
                let value = self.pop();
                if let HeapObject::Struct(fields) = &mut self.heap[struct_ref] {
                    fields[idx] = value;
                }
                self.push(value);
            }

            Inst::NewArray => {
                let size = self.pop() as usize;
                self.heap.push(HeapObject::Array(vec![0; size]));
                self.push((self.heap.len() - 1) as u64);
            }

            Inst::ArrayLen => {
                let arr_ref = self.pop() as usize;
                if let HeapObject::Array(items) = &self.heap[arr_ref] {
                    self.push(items.len() as u64);
                }
            }

            Inst::GetElem => {
                let idx = self.pop() as usize;
                let arr_ref = self.pop() as usize;
                if let HeapObject::Array(items) = &self.heap[arr_ref] {
                    self.push(items[idx]);
                }
            }

            Inst::SetElem => {
                let value = self.pop();
                let idx = self.pop() as usize;
                let arr_ref = self.pop() as usize;
                if let HeapObject::Array(items) = &mut self.heap[arr_ref] {
                    items[idx] = value;
                }
                self.push(value);
            }

            Inst::ConcatStr => {
                let idx2 = self.pop() as usize;
                let idx1 = self.pop() as usize;
                if let (HeapObject::String(s1), HeapObject::String(s2)) =
                    (&self.heap[idx1], &self.heap[idx2])
                {
                    let merged = format!("{}{}", s1, s2);
                    self.heap.push(HeapObject::String(merged));
                    self.push((self.heap.len() - 1) as u64);
                }
            }

            Inst::EqStr => {
                let idx2 = self.pop() as usize;
                let idx1 = self.pop() as usize;
                if let (HeapObject::String(s1), HeapObject::String(s2)) =
                    (&self.heap[idx1], &self.heap[idx2])
                {
                    self.push(if s1 == s2 { 1 } else { 0 });
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
                self.call_stack.push(Frame {
                    return_ptr: self.inst_ptr,
                    old_fp: self.fp,
                });
                self.fp = self.stack.len() - arg_count;
                self.inst_ptr = target;
            }

            Inst::Ret => {
                if let Some(frame) = self.call_stack.pop() {
                    self.stack.truncate(self.fp);
                    self.fp = frame.old_fp;
                    self.inst_ptr = frame.return_ptr;
                    self.push(0);
                } else {
                    self.done = true;
                    return Some(self.pop());
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
                if let HeapObject::String(s) = &self.heap[idx] {
                    println!("{}", s);
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
                        self.done = true;
                        return Some(VM_ERROR_SENTINEL);
                    }
                }
            }
        }

        None
    }
}
