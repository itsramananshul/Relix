use crate::sol::{
    analyzer::{Symbol, TypeTable},
    lexer::{Token, TokenKind},
    parser::{Ast, Program, Type},
};
use std::collections::HashMap;

struct Scope {
    variables: Vec<(String, Type)>,
}

#[derive(Debug, Clone)]
pub enum Inst {
    PushConst(Ast),
    StoreLocal(isize),
    LoadLocal(isize),
    Pop,
    Dup,

    IntAdd,
    IntSub,
    IntMul,
    IntDiv,
    IntEq,
    IntNeq,
    IntGt,
    IntLt,
    IntGte,
    IntLte,

    FloatAdd,
    FloatSub,
    FloatMul,
    FloatDiv,
    FloatEq,
    FloatNeq,
    FloatGt,
    FloatLt,
    FloatGte,
    FloatLte,

    CharEq,
    CharNeq,
    CharGt,
    CharLt,
    CharGte,
    CharLte,

    LogOr,
    LogAnd,
    LogNot,

    BitXor,
    BitAnd,
    BitOr,
    BitNeg,
    BitLShift,
    BitRShift,

    NewStruct(usize),
    GetField(usize),
    SetField(usize),

    NewArray,
    ArrayLen,
    GetElem,
    SetElem,

    ConcatStr,
    EqStr,

    Jump(usize),
    JumpFalse(usize),
    Call(usize, usize),
    Ret,
    RetVal,

    PrintInt,
    PrintFloat,
    PrintChar,
    PrintString,

    // ---- Relix extensions (M6) ----
    //
    // RemoteCall: invoke a capability on a remote peer.
    //
    // Stack contract (matching the verbatim port's `Call(target, argc)`
    // convention, pushed left-to-right):
    //   ... arg
    //   ... arg, method   (top)
    //   ... arg, method, peer
    //
    // Wait — convention is: push peer, then method, then arg. Top of stack
    // is the last-pushed value (arg). Pop order: arg, then method, then peer.
    //
    // All three are heap-string refs (`HeapObject::String`). The VM pops
    // them, resolves to strings, and dispatches via the registered
    // `RemoteCallDispatcher`. The response (bytes from the remote handler)
    // is pushed back as a new `HeapObject::String` ref.
    //
    // On failure the VM halts and `last_error()` returns the cause; `run()`
    // returns the error sentinel (`u64::MAX`).
    //
    // See `crate::sol::dispatcher` and `docs/sol-runtime-analysis.md`.
    RemoteCall,

    // RELIX-2 step 4: streaming variant of RemoteCall. Same
    // stack contract (pops peer / method / arg heap-string
    // refs in reverse-push order, pushes a heap-string ref
    // to the concatenated response body). At the VM layer
    // the only difference is which dispatcher method is
    // invoked: `remote_call_stream` instead of `remote_call`.
    // The VM still produces a single concatenated result so
    // SOL flows remain synchronous from the author's
    // perspective. External observers (web bridge SSE) hook
    // the per-chunk callback via [`VM::with_chunk_observer`]
    // to ship tokens to HTTP clients before the VM finishes.
    RemoteCallStream,

    // ---- F2 (try / catch / rethrow) ----
    //
    // TryEnter pushes a handler onto the VM's try-handler stack.
    // The handler carries the bytecode address of the catch
    // dispatch block — the point the VM jumps to when a
    // RemoteCall (or nested operation) fails. TryExit pops the
    // most recent handler when the try body completes
    // successfully.
    //
    // LoadErrorKind / LoadErrorCause / LoadErrorRetryHint push
    // the current error's classified kind (string),
    // human-readable cause (string), or retry_hint (int) onto
    // the stack. They are used inside catch dispatch blocks to
    // compare kinds and inside catch bodies to surface the
    // failure to the SOL author.
    //
    // Rethrow re-raises the current error, popping back through
    // outer try handlers (or halting with `VM_ERROR_SENTINEL`
    // when no outer handler exists).
    TryEnter(usize),
    TryExit,
    LoadErrorKind,
    LoadErrorCause,
    LoadErrorRetryHint,
    Rethrow,

    // ---- RELIX-7.19: last_confidence() builtin ----
    //
    // Zero-arg opcode that pushes the VM's `last_confidence`
    // field (an `f32` stored as the bit pattern of an `f64`)
    // onto the operand stack. Updated by the host AFTER each
    // `remote_call` returns; reads `1.0` before any call.
    LoadLastConfidence,

    // ---- F5 / F7: list & map opcodes ----
    //
    // Lists and maps are heap objects (`HeapObject::List` /
    // `HeapObject::Map`). The opcodes below pop the relevant
    // operands and push back either a heap ref to a freshly
    // built object (immutable update semantics — operators
    // bind the new ref to a variable) or a scalar result.
    //
    // `PushList(n)` pops `n` values from the top of the
    // stack, reverses them so iteration order matches push
    // order, and pushes a heap ref to the new list.
    //
    // `PushMap(n)` pops `2n` values (key1, val1, key2, val2,
    // ..., keyN, valN — alternating, key first in source
    // order, so the last-pushed value at the top of the
    // stack is `valN`) and pushes a heap ref to the new map.
    //
    // All list / map builtins are pure (no in-place
    // mutation) — the original heap object survives so
    // callers that aliased it are unaffected.
    PushList(usize),
    PushMap(usize),
    ListLen,
    ListGet,
    ListPush,
    ListContains,
    ListJoin,
    ListSplit,
    MapGet,
    MapSet,
    MapHas,
    MapKeys,
    MapLen,
    MapDel,

    // ---- F11: nested-typed accessors ----
    //
    // `ListGetList` is `ListGet` + a heap-object-type check:
    // the popped element MUST resolve to a `HeapObject::List`,
    // otherwise the VM halts with VM_ERROR_SENTINEL and a
    // structured `last_error` cause. Same shape for
    // `MapGetMap` against `HeapObject::Map`. Operators reach
    // for these when they want a compile-time-style guarantee
    // that the structure is what they expect — the failure
    // surface mirrors what `remote_call` does on a transport
    // failure, so existing `try / catch` flows can wrap the
    // call and recover.
    ListGetList,
    MapGetMap,
}

pub struct Codegen {
    type_tables: Vec<TypeTable>,
    locals: HashMap<String, (usize, Type)>,
    next_slot: usize,
    active_scopes: Vec<Scope>,
    functions: HashMap<String, usize>,
    fn_returns: HashMap<String, Type>,
    struct_layouts: HashMap<String, Vec<(String, Type)>>,
    for_loop_counter: usize,
    pending_calls: Vec<(usize, String)>,
}

impl Codegen {
    pub fn from(type_tables: Vec<TypeTable>) -> Self {
        Self {
            type_tables,
            locals: HashMap::new(),
            next_slot: 0,
            active_scopes: Vec::new(),
            functions: HashMap::new(),
            fn_returns: HashMap::new(),
            struct_layouts: HashMap::new(),
            for_loop_counter: 0,
            pending_calls: Vec::new(),
        }
    }

    fn scope_from(&self, scope_id: usize) -> Scope {
        let mut scope = Scope {
            variables: Vec::new(),
        };
        if scope_id < self.type_tables.len() {
            for (name, sym) in self.type_tables[scope_id].clone() {
                match sym {
                    Symbol::Variable { kind } => scope.variables.push((name.to_owned(), *kind)),
                    _ => continue,
                }
            }
        }
        scope
    }

    pub fn gen_bcode(&mut self, program: &Program) -> Vec<Inst> {
        for node in program {
            if let Ast::DeclStruct { name, fields } = node {
                let mut sorted_fields: Vec<(String, Type)> =
                    fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                sorted_fields.sort_by(|a, b| a.0.cmp(&b.0));
                self.struct_layouts.insert(name.clone(), sorted_fields);
            }
        }

        let mut insts = Vec::new();
        for node in program {
            let is_expr = self.is_expression_node(node);
            self.compile(&mut insts, node.clone());

            if is_expr {
                insts.push(Inst::Pop);
            }
        }

        for (inst_idx, name) in &self.pending_calls {
            if let Some(&addr) = self.functions.get(name) {
                if let Inst::Call(_, count) = insts[*inst_idx] {
                    insts[*inst_idx] = Inst::Call(addr, count);
                }
            }
        }

        if let Some(&start_addr) = self.functions.get("start") {
            insts.push(Inst::Call(start_addr, 0));
        }

        insts
    }

    fn is_expression_node(&self, node: &Ast) -> bool {
        matches!(
            node,
            Ast::ExprBinary { .. }
                | Ast::ExprUnary { .. }
                | Ast::ExprFuncCall { .. }
                | Ast::ExprMemAcc { .. }
                | Ast::ExprArrAcc { .. }
                | Ast::ExprInteger(_)
                | Ast::ExprFloat(_)
                | Ast::ExprString(_)
                | Ast::ExprChar(_)
                | Ast::ExprBool(_)
                | Ast::ExprVar(_)
                | Ast::ExprStructInit { .. }
                | Ast::ExprArrayInit { .. }
                | Ast::ExprList { .. }
                | Ast::ExprMap { .. }
                | Ast::ExprEnumVar { .. }
                | Ast::ExprReturn { .. }
                | Ast::ExprUndefined
        )
    }

    fn compile(&mut self, insts: &mut Vec<Inst>, node: Ast) {
        match node {
            // --- 1. Primitive Constants ---
            Ast::ExprInteger(v) => insts.push(Inst::PushConst(Ast::ExprInteger(v))),
            Ast::ExprFloat(v) => insts.push(Inst::PushConst(Ast::ExprFloat(v))),
            Ast::ExprChar(v) => insts.push(Inst::PushConst(Ast::ExprChar(v))),
            Ast::ExprString(v) => insts.push(Inst::PushConst(Ast::ExprString(v))),
            Ast::ExprBool(v) => insts.push(Inst::PushConst(Ast::ExprBool(v))),
            Ast::ExprUndefined => insts.push(Inst::PushConst(Ast::ExprUndefined)),

            // --- 2. Variables & Declarations ---
            Ast::ExprVar(name) => {
                let offset = self.find_local_offset(&name);
                insts.push(Inst::LoadLocal(offset));
            }
            Ast::DeclVar { name, kind, value } => {
                if let Some(val_node) = value {
                    self.compile(insts, *val_node);
                    let offset = self.get_or_create_local(&name, kind);
                    insts.push(Inst::StoreLocal(offset));
                } else {
                    self.get_or_create_local(&name, kind);
                }
            }
            Ast::ExprAssign { var_name, value } => {
                self.compile(insts, *value);
                insts.push(Inst::Dup);
                let offset = self.find_local_offset(&var_name);
                insts.push(Inst::StoreLocal(offset));
            }

            // --- 3. Blocks & Local Statements ---
            Ast::Block {
                block,
                scope: scope_id,
            } => {
                if scope_id < self.type_tables.len() {
                    self.active_scopes.push(self.scope_from(scope_id));
                }

                let saved_next = self.next_slot;

                for stmt in block {
                    let is_expr = self.is_expression_node(&stmt);
                    self.compile(insts, stmt);
                    if is_expr {
                        insts.push(Inst::Pop);
                    }
                }

                self.locals.retain(|_, (slot, _)| *slot < saved_next);
                self.next_slot = saved_next;

                if scope_id < self.type_tables.len() {
                    self.active_scopes.pop();
                }
            }

            // --- 4. Control Flow ---
            Ast::StmtIf {
                condition,
                body,
                alt,
            } => {
                self.compile(insts, *condition);
                let jump_false_idx = insts.len();
                insts.push(Inst::JumpFalse(0));

                self.compile(insts, *body);

                if let Some(else_branch) = alt {
                    let jump_end_idx = insts.len();
                    insts.push(Inst::Jump(0));

                    let else_start = insts.len();
                    insts[jump_false_idx] = Inst::JumpFalse(else_start);

                    self.compile(insts, *else_branch);
                    let end_idx = insts.len();
                    insts[jump_end_idx] = Inst::Jump(end_idx);
                } else {
                    let end_idx = insts.len();
                    insts[jump_false_idx] = Inst::JumpFalse(end_idx);
                }
            }

            Ast::StmtWhile { condition, body } => {
                let loop_start = insts.len();
                self.compile(insts, *condition);

                let jump_false_idx = insts.len();
                insts.push(Inst::JumpFalse(0));

                self.compile(insts, *body);
                insts.push(Inst::Jump(loop_start));

                let loop_end = insts.len();
                insts[jump_false_idx] = Inst::JumpFalse(loop_end);
            }

            Ast::StmtFor {
                elem_name,
                array,
                body,
            } => {
                let saved_next = self.next_slot;

                // F5: pick the matching len / get opcodes
                // based on the iterable's type. Typed arrays
                // keep the original ArrayLen / GetElem path;
                // lists use the F5 ListLen / ListGet pair.
                let array_type = self.infer_type(&array);
                let (len_op, get_op, elem_type) = match array_type {
                    Type::Array { ref inner, .. } => {
                        (Inst::ArrayLen, Inst::GetElem, (**inner).clone())
                    }
                    Type::List => (Inst::ListLen, Inst::ListGet, Type::String),
                    _ => (Inst::ArrayLen, Inst::GetElem, Type::Integer),
                };
                self.get_or_create_local(&elem_name, elem_type);

                let counter = self.for_loop_counter;
                self.for_loop_counter += 1;
                let arr_slot = format!("__for_arr_{}", counter);
                let idx_slot = format!("__for_idx_{}", counter);
                let len_slot = format!("__for_len_{}", counter);

                self.compile(insts, *array);
                self.get_or_create_local(&arr_slot, Type::Integer);
                insts.push(Inst::StoreLocal(self.find_local_offset(&arr_slot)));

                insts.push(Inst::LoadLocal(self.find_local_offset(&arr_slot)));
                insts.push(len_op);
                self.get_or_create_local(&len_slot, Type::Integer);
                insts.push(Inst::StoreLocal(self.find_local_offset(&len_slot)));

                insts.push(Inst::PushConst(Ast::ExprInteger(0)));
                self.get_or_create_local(&idx_slot, Type::Integer);
                insts.push(Inst::StoreLocal(self.find_local_offset(&idx_slot)));

                let loop_start = insts.len();

                insts.push(Inst::LoadLocal(self.find_local_offset(&idx_slot)));
                insts.push(Inst::LoadLocal(self.find_local_offset(&len_slot)));
                insts.push(Inst::IntLt);
                let jump_false_idx = insts.len();
                insts.push(Inst::JumpFalse(0));

                insts.push(Inst::LoadLocal(self.find_local_offset(&arr_slot)));
                insts.push(Inst::LoadLocal(self.find_local_offset(&idx_slot)));
                insts.push(get_op);
                insts.push(Inst::StoreLocal(self.find_local_offset(&elem_name)));

                self.compile(insts, *body);

                insts.push(Inst::LoadLocal(self.find_local_offset(&idx_slot)));
                insts.push(Inst::PushConst(Ast::ExprInteger(1)));
                insts.push(Inst::IntAdd);
                insts.push(Inst::StoreLocal(self.find_local_offset(&idx_slot)));

                insts.push(Inst::Jump(loop_start));

                let loop_end = insts.len();
                insts[jump_false_idx] = Inst::JumpFalse(loop_end);

                self.locals.retain(|_, (slot, _)| *slot < saved_next);
                self.next_slot = saved_next;
            }

            // --- F2: try / catch / rethrow ---
            //
            // Codegen layout for `try { B } catch k1 { C1 } catch k2 { C2 }`:
            //
            //     TryEnter dispatch_pc
            //     <B>
            //     TryExit
            //     Jump end_pc
            //   dispatch_pc:                   ; VM lands here on B failure
            //                                  ; last_error is set
            //                                  ; (handler already popped)
            //     PushConst "k1"
            //     LoadErrorKind
            //     EqStr
            //     JumpFalse skip_k1
            //     <C1>
            //     Jump end_pc
            //   skip_k1:
            //     PushConst "k2"
            //     LoadErrorKind
            //     EqStr
            //     JumpFalse skip_k2
            //     <C2>
            //     Jump end_pc
            //   skip_k2:
            //     Rethrow                       ; no catch matched
            //   end_pc:
            //
            // The "any" kind skips the EqStr + JumpFalse pair —
            // it unconditionally executes its body.
            Ast::StmtTry { body, catches } => {
                let try_enter_idx = insts.len();
                insts.push(Inst::TryEnter(0));
                self.compile(insts, *body);
                insts.push(Inst::TryExit);
                let jump_to_end_after_body = insts.len();
                insts.push(Inst::Jump(0));

                let dispatch_pc = insts.len();
                insts[try_enter_idx] = Inst::TryEnter(dispatch_pc);

                let mut jump_to_end_indices: Vec<usize> = Vec::new();
                jump_to_end_indices.push(jump_to_end_after_body);

                for (kind, catch_body) in catches.into_iter() {
                    if kind == "any" {
                        // Unconditional match — emit body and
                        // jump to end. Any subsequent catch
                        // clauses are unreachable but we still
                        // compile them so error messages stay
                        // honest.
                        self.compile(insts, catch_body);
                        let j = insts.len();
                        insts.push(Inst::Jump(0));
                        jump_to_end_indices.push(j);
                        continue;
                    }
                    insts.push(Inst::PushConst(Ast::ExprString(kind.clone())));
                    insts.push(Inst::LoadErrorKind);
                    insts.push(Inst::EqStr);
                    let jump_skip_idx = insts.len();
                    insts.push(Inst::JumpFalse(0));
                    self.compile(insts, catch_body);
                    let j = insts.len();
                    insts.push(Inst::Jump(0));
                    jump_to_end_indices.push(j);
                    let skip_pc = insts.len();
                    insts[jump_skip_idx] = Inst::JumpFalse(skip_pc);
                }
                insts.push(Inst::Rethrow);

                let end_pc = insts.len();
                for idx in jump_to_end_indices {
                    insts[idx] = Inst::Jump(end_pc);
                }
            }

            Ast::StmtRethrow => {
                insts.push(Inst::Rethrow);
            }

            // --- 5. Operations & Intercepted Assignments ---
            Ast::ExprBinary { lhs, rhs, op } => {
                if op.get_kind() == TokenKind::Eq {
                    match *lhs {
                        Ast::ExprVar(name) => {
                            self.compile(insts, *rhs);
                            insts.push(Inst::Dup);
                            let offset = self.find_local_offset(&name);
                            insts.push(Inst::StoreLocal(offset));
                        }
                        Ast::ExprArrAcc { lhs: arr, index } => {
                            self.compile(insts, *arr);
                            self.compile(insts, *index);
                            self.compile(insts, *rhs);
                            insts.push(Inst::SetElem);
                        }
                        Ast::ExprMemAcc { lhs: obj, member } => {
                            self.compile(insts, *rhs);
                            insts.push(Inst::Dup);

                            let obj_type = self.infer_type(&obj);
                            self.compile(insts, *obj);

                            let mut field_idx = 0;
                            if let Type::Ident(struct_name) = obj_type {
                                if let Some(layout) = self.struct_layouts.get(&struct_name) {
                                    if let Some(pos) = layout.iter().position(|(n, _)| n == &member)
                                    {
                                        field_idx = pos;
                                    }
                                }
                            }
                            insts.push(Inst::SetField(field_idx));
                        }
                        _ => panic!("Compile Error: Invalid left-hand side assignment target."),
                    }
                } else {
                    self.compile(insts, *lhs.clone());
                    self.compile(insts, *rhs.clone());
                    let ty = self.infer_type(&Ast::ExprBinary {
                        lhs,
                        rhs,
                        op: op.clone(),
                    });
                    self.emit_binary_op(insts, op, ty);
                }
            }

            Ast::ExprUnary { child, op } => {
                self.compile(insts, *child.clone());
                let op_kind = op.get_kind();
                match op_kind {
                    TokenKind::Bang => insts.push(Inst::LogNot),
                    TokenKind::Tilde => insts.push(Inst::BitNeg),
                    TokenKind::Dash => {
                        if let Type::Float = self.infer_type(&child) {
                            insts.push(Inst::PushConst(Ast::ExprFloat(-1.0)));
                            insts.push(Inst::FloatMul);
                        } else {
                            insts.push(Inst::PushConst(Ast::ExprInteger(-1)));
                            insts.push(Inst::IntMul);
                        }
                    }
                    _ => {}
                }
            }

            // --- 6. Functions & Calls ---
            Ast::DeclFunc {
                name,
                params,
                ret,
                body,
                scope,
                ..
            } => {
                let jump_over_idx = insts.len();
                insts.push(Inst::Jump(0));

                let func_entry = insts.len();
                self.functions.insert(name.clone(), func_entry);
                self.fn_returns.insert(name, ret);

                self.locals.clear();
                self.next_slot = 0;

                for (param_name, param_type) in params {
                    self.locals.insert(param_name, (self.next_slot, param_type));
                    self.next_slot += 1;
                }

                if scope < self.type_tables.len() {
                    self.active_scopes.push(self.scope_from(scope));
                }

                self.compile(insts, *body);
                insts.push(Inst::Ret);

                if scope < self.type_tables.len() {
                    self.active_scopes.pop();
                }

                let end_idx = insts.len();
                insts[jump_over_idx] = Inst::Jump(end_idx);
            }
            Ast::ExprFuncCall { name, args } => {
                if name == "print" && !args.is_empty() {
                    self.compile(insts, args[0].clone());
                    match self.display_type(&args[0]) {
                        Type::Integer | Type::Bool => insts.push(Inst::PrintInt),
                        Type::Float => insts.push(Inst::PrintFloat),
                        Type::Char => insts.push(Inst::PrintChar),
                        Type::String => insts.push(Inst::PrintString),
                        _ => insts.push(Inst::PrintInt),
                    }
                } else if name == "remote_call" {
                    // Relix M6 extension: emit each arg expression in source order
                    // (peer, method, arg), then a single RemoteCall opcode. The
                    // analyzer has already validated arity (3) and arg types (all str).
                    for arg in args {
                        self.compile(insts, arg);
                    }
                    insts.push(Inst::RemoteCall);
                } else if name == "remote_call_stream" {
                    // RELIX-2 step 4: streaming variant. Same
                    // stack contract as remote_call — three
                    // string args pushed in source order,
                    // then a single RemoteCallStream opcode.
                    // The analyzer has already validated
                    // arity + arg types.
                    for arg in args {
                        self.compile(insts, arg);
                    }
                    insts.push(Inst::RemoteCallStream);
                } else if name == "error_kind" {
                    insts.push(Inst::LoadErrorKind);
                } else if name == "error_cause" {
                    insts.push(Inst::LoadErrorCause);
                } else if name == "error_retry_hint" {
                    insts.push(Inst::LoadErrorRetryHint);
                } else if name == "last_confidence" {
                    // RELIX-7.19: zero-arg builtin. Returns the
                    // VM's confidence register (set by the host
                    // after each remote_call) as a Float.
                    insts.push(Inst::LoadLastConfidence);
                } else if matches!(
                    name.as_str(),
                    "list_len"
                        | "list_get"
                        | "list_get_list"
                        | "list_push"
                        | "list_contains"
                        | "list_join"
                        | "list_split"
                        | "map_get"
                        | "map_get_map"
                        | "map_set"
                        | "map_has"
                        | "map_keys"
                        | "map_len"
                        | "map_del"
                ) {
                    // F6 / F8: list & map built-ins. Each one
                    // pushes its arguments left-to-right then
                    // emits the matching opcode. Arity has
                    // already been validated by the analyzer
                    // (panic on mismatch), so we trust it here.
                    for arg in args {
                        self.compile(insts, arg);
                    }
                    let op = match name.as_str() {
                        "list_len" => Inst::ListLen,
                        "list_get" => Inst::ListGet,
                        "list_get_list" => Inst::ListGetList,
                        "list_push" => Inst::ListPush,
                        "list_contains" => Inst::ListContains,
                        "list_join" => Inst::ListJoin,
                        "list_split" => Inst::ListSplit,
                        "map_get" => Inst::MapGet,
                        "map_get_map" => Inst::MapGetMap,
                        "map_set" => Inst::MapSet,
                        "map_has" => Inst::MapHas,
                        "map_keys" => Inst::MapKeys,
                        "map_len" => Inst::MapLen,
                        "map_del" => Inst::MapDel,
                        _ => unreachable!(),
                    };
                    insts.push(op);
                } else if let Some(&target_address) = self.functions.get(&name) {
                    let count = args.len();
                    for arg in args {
                        self.compile(insts, arg);
                    }
                    insts.push(Inst::Call(target_address, count));
                } else {
                    let count = args.len();
                    for arg in args {
                        self.compile(insts, arg);
                    }
                    let inst_idx = insts.len();
                    insts.push(Inst::Call(0, count));
                    self.pending_calls.push((inst_idx, name));
                }
            }
            Ast::ExprReturn { val } => {
                if let Some(ret_node) = val {
                    self.compile(insts, *ret_node);
                    insts.push(Inst::RetVal);
                } else {
                    insts.push(Inst::Ret);
                }
            }

            // --- 7. Compounds (Structs, Arrays, Enums) ---
            // FIX: Added .cloned() to safely dissociate the map reference from the recursive call loops
            Ast::ExprStructInit { name, fields } => {
                if let Some(layout) = self.struct_layouts.get(&name).cloned() {
                    for (f_name, _) in &layout {
                        if let Some((_, f_val)) = fields.iter().find(|(n, _)| n == f_name) {
                            self.compile(insts, f_val.clone());
                        } else {
                            insts.push(Inst::PushConst(Ast::ExprUndefined));
                        }
                    }
                    insts.push(Inst::NewStruct(layout.len()));
                } else {
                    insts.push(Inst::NewStruct(0));
                }
            }
            Ast::ExprMemAcc { lhs, member } => {
                let lhs_type = self.infer_type(&lhs);
                self.compile(insts, *lhs);

                let mut field_idx = 0;
                if let Type::Ident(struct_name) = lhs_type {
                    if let Some(layout) = self.struct_layouts.get(&struct_name) {
                        if let Some(pos) = layout.iter().position(|(n, _)| n == &member) {
                            field_idx = pos;
                        }
                    }
                }
                insts.push(Inst::GetField(field_idx));
            }
            Ast::ExprArrayInit { values } => {
                insts.push(Inst::PushConst(Ast::ExprInteger(values.len() as i128)));
                insts.push(Inst::NewArray);
                for (i, val) in values.into_iter().enumerate() {
                    insts.push(Inst::Dup);
                    insts.push(Inst::PushConst(Ast::ExprInteger(i as i128)));
                    self.compile(insts, val);
                    insts.push(Inst::SetElem);
                    insts.push(Inst::Pop);
                }
            }
            // F5 / F7: list / map literals.
            Ast::ExprList { elements } => {
                let n = elements.len();
                for elem in elements {
                    self.compile(insts, elem);
                }
                insts.push(Inst::PushList(n));
            }
            Ast::ExprMap { pairs } => {
                let n = pairs.len();
                for (key, value) in pairs {
                    insts.push(Inst::PushConst(Ast::ExprString(key)));
                    self.compile(insts, value);
                }
                insts.push(Inst::PushMap(n));
            }
            Ast::ExprArrAcc { lhs, index } => {
                self.compile(insts, *lhs);
                self.compile(insts, *index);
                insts.push(Inst::GetElem);
            }
            Ast::ExprEnumVar { var, .. } => {
                let variant_hash = var.chars().next().unwrap_or('A') as i128 % 10;
                insts.push(Inst::PushConst(Ast::ExprInteger(variant_hash)));
            }

            // --- Compile-time Meta Nodes (No Op) ---
            Ast::DeclStruct { .. } | Ast::DeclEnum { .. } | Ast::StmtImport { .. } => {}
        }
    }

    fn get_or_create_local(&mut self, name: &str, ty: Type) -> isize {
        if let Some((slot, _)) = self.locals.get(name) {
            *slot as isize
        } else {
            let slot = self.next_slot;
            self.locals.insert(name.to_string(), (slot, ty));
            self.next_slot += 1;
            slot as isize
        }
    }

    fn find_local_offset(&mut self, name: &str) -> isize {
        if let Some((slot, _)) = self.locals.get(name) {
            *slot as isize
        } else {
            let mut resolved_type = Type::Integer;
            for table in &self.type_tables {
                for (sym_name, sym) in table {
                    if sym_name == name {
                        if let Symbol::Variable { kind } = sym {
                            resolved_type = *kind.clone();
                        }
                    }
                }
            }
            let slot = self.next_slot;
            self.locals.insert(name.to_string(), (slot, resolved_type));
            self.next_slot += 1;
            slot as isize
        }
    }

    fn infer_type(&self, node: &Ast) -> Type {
        match node {
            Ast::ExprInteger(_) => Type::Integer,
            Ast::ExprFloat(_) => Type::Float,
            Ast::ExprChar(_) => Type::Char,
            Ast::ExprString(_) => Type::String,
            Ast::ExprBool(_) => Type::Bool,
            Ast::ExprVar(name) => {
                if let Some((_, ty)) = self.locals.get(name) {
                    return ty.clone();
                }
                for table in &self.type_tables {
                    for (sym_name, sym) in table {
                        if sym_name == name {
                            if let Symbol::Variable { kind } = sym {
                                return *kind.clone();
                            }
                        }
                    }
                }
                Type::Integer
            }
            Ast::ExprMemAcc { lhs, member } => {
                let lhs_type = self.infer_type(lhs);
                if let Type::Ident(struct_name) = lhs_type {
                    if let Some(layout) = self.struct_layouts.get(&struct_name) {
                        if let Some((_, f_type)) = layout.iter().find(|(n, _)| n == member) {
                            return f_type.clone();
                        }
                    }
                }
                Type::Integer
            }
            Ast::ExprBinary { lhs, rhs, .. } => {
                let lt = self.infer_type(lhs);
                let rt = self.infer_type(rhs);
                if let Type::Float = lt {
                    Type::Float
                } else if let Type::Float = rt {
                    Type::Float
                } else {
                    lt
                }
            }
            Ast::ExprUnary { child, .. } => self.infer_type(child),
            Ast::ExprArrAcc { lhs, .. } => match self.infer_type(lhs) {
                Type::Array { inner, .. } => *inner,
                _ => Type::Integer,
            },
            Ast::ExprFuncCall { name, .. } => {
                // Relix M6: `remote_call` is a known builtin that returns String.
                if name == "remote_call" {
                    return Type::String;
                }
                // RELIX-2 step 4: streaming variant also
                // returns String (concatenated chunks).
                if name == "remote_call_stream" {
                    return Type::String;
                }
                // F2 built-ins exposing the current error inside a catch.
                if name == "error_kind" || name == "error_cause" {
                    return Type::String;
                }
                if name == "error_retry_hint" {
                    return Type::Integer;
                }
                // RELIX-7.19: confidence accessor.
                if name == "last_confidence" {
                    return Type::Float;
                }
                // F6 / F8: list & map built-ins.
                match name.as_str() {
                    "list_len" | "map_len" => return Type::Integer,
                    "list_get" | "list_join" | "map_get" => return Type::String,
                    "list_contains" | "map_has" => return Type::Bool,
                    "list_push" | "list_split" | "map_keys" | "list_get_list" => {
                        return Type::List;
                    }
                    "map_set" | "map_del" | "map_get_map" => return Type::Map,
                    _ => {}
                }
                self.fn_returns.get(name).cloned().unwrap_or(Type::Integer)
            }
            _ => Type::Integer,
        }
    }

    fn display_type(&self, node: &Ast) -> Type {
        match node {
            Ast::ExprBinary { op, .. } => match op.get_kind() {
                TokenKind::EqEq
                | TokenKind::BangEq
                | TokenKind::MoreThan
                | TokenKind::LessThan
                | TokenKind::MoreEq
                | TokenKind::LessEq
                | TokenKind::PipePipe
                | TokenKind::AmpAmp => Type::Integer,
                _ => self.infer_type(node),
            },
            Ast::ExprUnary { op, .. } => match op.get_kind() {
                TokenKind::Bang => Type::Integer,
                _ => self.infer_type(node),
            },
            _ => self.infer_type(node),
        }
    }

    fn emit_binary_op(&self, insts: &mut Vec<Inst>, op: Token, ty: Type) {
        let op_kind = op.get_kind();
        match ty {
            Type::Float => match op_kind {
                TokenKind::Plus => insts.push(Inst::FloatAdd),
                TokenKind::Dash => insts.push(Inst::FloatSub),
                TokenKind::Star => insts.push(Inst::FloatMul),
                TokenKind::Slash => insts.push(Inst::FloatDiv),
                TokenKind::EqEq => insts.push(Inst::FloatEq),
                TokenKind::BangEq => insts.push(Inst::FloatNeq),
                TokenKind::MoreThan => insts.push(Inst::FloatGt),
                TokenKind::LessThan => insts.push(Inst::FloatLt),
                TokenKind::MoreEq => insts.push(Inst::FloatGte),
                TokenKind::LessEq => insts.push(Inst::FloatLte),
                _ => {}
            },
            Type::Char => match op_kind {
                TokenKind::EqEq => insts.push(Inst::CharEq),
                TokenKind::BangEq => insts.push(Inst::CharNeq),
                TokenKind::MoreThan => insts.push(Inst::CharGt),
                TokenKind::LessThan => insts.push(Inst::CharLt),
                TokenKind::MoreEq => insts.push(Inst::CharGte),
                TokenKind::LessEq => insts.push(Inst::CharLte),
                _ => {}
            },
            Type::String => match op_kind {
                TokenKind::Plus => insts.push(Inst::ConcatStr),
                TokenKind::EqEq => insts.push(Inst::EqStr),
                TokenKind::BangEq => {
                    insts.push(Inst::EqStr);
                    insts.push(Inst::LogNot);
                }
                _ => {}
            },
            _ => match op_kind {
                TokenKind::Plus => insts.push(Inst::IntAdd),
                TokenKind::Dash => insts.push(Inst::IntSub),
                TokenKind::Star => insts.push(Inst::IntMul),
                TokenKind::Slash => insts.push(Inst::IntDiv),
                TokenKind::EqEq => insts.push(Inst::IntEq),
                TokenKind::BangEq => insts.push(Inst::IntNeq),
                TokenKind::MoreThan => insts.push(Inst::IntGt),
                TokenKind::LessThan => insts.push(Inst::IntLt),
                TokenKind::MoreEq => insts.push(Inst::IntGte),
                TokenKind::LessEq => insts.push(Inst::IntLte),
                TokenKind::PipePipe => insts.push(Inst::LogOr),
                TokenKind::AmpAmp => insts.push(Inst::LogAnd),
                TokenKind::Caret => insts.push(Inst::BitXor),
                TokenKind::Ampersand => insts.push(Inst::BitAnd),
                TokenKind::Pipe => insts.push(Inst::BitOr),
                TokenKind::LShift => insts.push(Inst::BitLShift),
                TokenKind::RShift => insts.push(Inst::BitRShift),
                _ => {}
            },
        }
    }
}
