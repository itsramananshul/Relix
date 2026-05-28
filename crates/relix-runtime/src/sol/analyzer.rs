use std::collections::HashMap;

use crate::sol::{
    lexer::Token,
    parser::{Ast, Program, Type},
    util::type_eq,
};

#[derive(Debug, Clone)]
pub enum Symbol {
    Variable { kind: Box<Type> },
    Enum { variants: HashMap<String, isize> },
    Struct { fields: HashMap<String, Box<Type>> },
}
pub type TypeTableId = usize;
pub type TypeTable = HashMap<String, Symbol>;

pub struct Analyzer {
    pub tt_arena: Vec<TypeTable>,
    tts: Vec<TypeTableId>,
    can_break: bool,
    can_return: bool,
    /// Declared return type of the function being analyzed.
    /// Set on entry to a `DeclFunc`, restored on exit. Used
    /// by `ExprReturn` to validate every return statement —
    /// including the ones buried inside `if` / `else` /
    /// `while` / `for` / `try` / `catch` bodies — against the
    /// function's signature. `None` outside any function
    /// (top-level walk before the body is entered).
    return_type: Option<Type>,
}

impl Analyzer {
    pub fn new() -> Self {
        Self {
            tt_arena: Vec::new(),
            tts: Vec::new(),
            can_break: false,
            can_return: false,
            return_type: None,
        }
    }

    fn new_table(&mut self) -> TypeTableId {
        let id = self.tt_arena.len();
        self.tt_arena.push(HashMap::new());
        self.tts.push(id);
        id
    }
    fn pop_table(&mut self) {
        self.tts.pop();
    }
    fn add_entry(&mut self, name: String, symbol: Symbol) {
        if self.tt_arena.is_empty() {
            self.new_table();
        }

        let id = self.tts.last().unwrap();
        if self.tt_arena[*id]
            .insert(name.clone(), symbol.clone())
            .is_some()
        {
            panic!("\x1b[0;31merror\x1b[0;0m: redefinition of `{}`", name);
        }

        // eprintln!("[DEBUG] added {name} as {symbol:?}");
    }
    fn get_entry(&mut self, name: &String) -> Option<&Symbol> {
        let tables = self.tts.iter().map(|i| &self.tt_arena[*i]);
        tables.rev().find_map(|table| table.get(name))
    }

    // fn kindof(&self, node: Ast) -> Type {
    //     Type::
    // }

    pub fn run(&mut self, program: &mut Program) {
        self.new_table(); // globals

        // Pass 1: Register all top-level function declarations so forward references work
        for decl in program.iter() {
            if let Ast::DeclFunc {
                name, params, ret, ..
            } = decl
            {
                let function_type = Box::new(Type::Function {
                    params: params.iter().map(|(_, ty)| ty.to_owned()).collect(),
                    ret: Box::new(ret.clone()),
                });
                self.add_entry(
                    name.to_owned(),
                    Symbol::Variable {
                        kind: function_type,
                    },
                );
            }
        }

        // Pass 2: Check all declarations (function names already known)
        for decl in program {
            self.check(decl);
        }
    }
    fn check(&mut self, node: &mut Ast) -> Option<Type> {
        match node {
            Ast::DeclFunc {
                name,
                params,
                ret,
                body,
                scope,
            } => {
                let function_type = Box::new(Type::Function {
                    params: params.iter().map(|(_, ty)| ty.to_owned()).collect(),
                    ret: Box::new(ret.clone()),
                });
                // Already registered in run() pass 1 — no need for add_entry

                *scope = self.new_table();
                for (pname, kind) in params {
                    self.add_entry(
                        pname.to_owned(),
                        Symbol::Variable {
                            kind: Box::from(kind.clone()),
                        },
                    );
                }

                // Branch return-type checker. Two pieces:
                //
                //   1. Every `ExprReturn` in the body
                //      (including ones buried inside if /
                //      else / while / for / try / catch) is
                //      type-checked against the declared
                //      return type via
                //      `Analyzer::return_type`.
                //   2. After the body is analysed, a
                //      conservative `block_always_returns`
                //      walk asserts the function guarantees a
                //      return on every path. `Void` functions
                //      are exempt — falling off the end is a
                //      valid no-value exit there.
                //
                // The coverage walk is intentionally
                // permissive: a `while` / `for` body that
                // returns doesn't count (the loop might not
                // execute), and a `try` is treated as
                // returning only when the try body and every
                // catch body both return. We don't try to
                // prove exhaustive coverage past `if/else`
                // and `try` — that's a much harder problem
                // and the user spec calls for conservative.
                let old_can_return = self.can_return;
                let old_return_type = self.return_type.take();
                self.can_return = true;
                self.return_type = Some(ret.clone());
                self.check(body);
                if !matches!(ret, Type::Void) && !block_always_returns(body) {
                    panic!(
                        "function `{name}` is declared as returning `{ret:?}` but its body \
                         does not guarantee a return on all paths"
                    );
                }
                self.can_return = old_can_return;
                self.return_type = old_return_type;

                self.pop_table();
                Some(*function_type)
            }
            Ast::DeclVar { name, kind, .. } => {
                self.add_entry(
                    name.to_owned(),
                    Symbol::Variable {
                        kind: Box::new(kind.clone()),
                    },
                );
                Some(kind.clone())
            }
            Ast::DeclStruct { name, fields } => {
                self.add_entry(
                    name.to_owned(),
                    Symbol::Struct {
                        fields: fields
                            .iter()
                            .map(|(name, ty)| (name.to_owned(), Box::from(ty.clone())))
                            .collect(),
                    },
                );
                Some(Type::Ident(name.clone()))
            }
            Ast::DeclEnum { name, variants } => {
                self.add_entry(
                    name.to_owned(),
                    Symbol::Enum {
                        variants: variants.to_owned(),
                    },
                );
                Some(Type::Ident(name.clone()))
            }
            Ast::Block {
                block: stmts,
                scope,
            } => {
                if stmts.len() == 0 {
                    return Some(Type::Void);
                }
                *scope = self.new_table();

                let mut last = None;
                for stmt in stmts {
                    let ty = self.check(stmt)?;
                    // if last.is_some() && type_eq(ty.clone(), last.clone().unwrap()).is_err() {
                    //     return None;
                    // }
                    last = Some(ty);
                }

                self.pop_table();
                last
            }
            Ast::StmtImport { alias, .. } => {
                if let Some(a) = alias {
                    self.add_entry(
                        a.to_owned(),
                        Symbol::Variable {
                            kind: Box::from(Type::Void),
                        },
                    );
                }
                Some(Type::Void)
            }
            Ast::StmtIf {
                condition,
                body,
                alt,
            } => {
                let cond = self.check(condition)?;
                if type_eq(cond.clone(), Type::Bool).is_err() {
                    panic!(
                        "condition of if statement must be of type `bool`, got {:?}",
                        cond
                    );
                }
                self.check(body);
                match alt {
                    Some(alt_block) => {
                        self.check(alt_block);
                    }
                    None => {}
                }

                Some(Type::Void)
            }
            Ast::StmtWhile { condition, body } => {
                let cond = self.check(&mut *condition)?;
                if type_eq(cond.clone(), Type::Bool).is_err() {
                    panic!(
                        "condition of if statement must be of type `bool`, got {:?}",
                        cond
                    );
                }
                let old = self.can_break;
                self.can_break = true;
                self.check(body);
                self.can_break = old;

                Some(Type::Void)
            }
            Ast::StmtTry { body, catches } => {
                self.check(body);
                for (_kind, catch_body) in catches.iter_mut() {
                    self.check(catch_body);
                }
                Some(Type::Void)
            }
            Ast::StmtRethrow => Some(Type::Void),
            Ast::StmtFor {
                elem_name,
                array,
                body,
            } => {
                let Some(arr_type) = self.check(array) else {
                    panic!(
                        "for-loop iterable must have a known type (list, array, or typed array)"
                    );
                };
                // F5: for-in now also accepts `list`. Each
                // element is exposed to the loop body as
                // `str` (the practical case operators use).
                // Typed `[N]T` arrays keep their element type.
                let elem_type: Box<Type> = match arr_type {
                    Type::Array { inner, .. } => inner,
                    Type::List => Box::new(Type::String),
                    other => panic!("for-loop iterable must be a list or array, got {other:?}"),
                };

                self.add_entry(elem_name.to_owned(), Symbol::Variable { kind: elem_type });
                let old = self.can_break;
                self.can_break = true;
                self.check(body);
                self.can_break = old;

                Some(Type::Void)
            }
            Ast::ExprAssign { var_name, value } => {
                let var_type = {
                    let entry = self.get_entry(&var_name).unwrap_or_else(|| {
                        panic!("variable `{var_name}` is assigned to before initialization");
                    });

                    if let Symbol::Variable { kind: var_type } = entry {
                        var_type.clone()
                    } else {
                        panic!(
                            "`{var_name}` is assigned to, however it is not a variable\n\t{entry:?}"
                        );
                    }
                };

                let rhs_type = self.check(value)?;
                if type_eq(*var_type.clone(), rhs_type.clone()).is_err() {
                    panic!("variable `{var_name}` is assigned to before initialization");
                }
                Some(rhs_type)
            }
            Ast::ExprBinary { lhs, rhs, op } => {
                let lhs_type = self.check(lhs)?;
                let rhs_type = self.check(rhs)?;

                match op {
                    // Arithmetic Operations
                    Token::Plus | Token::Dash | Token::Star | Token::Slash => {
                        if type_eq(lhs_type.clone(), rhs_type.clone()).is_err() {
                            panic!(
                                "mismatched types in arithmetic: {lhs_type:?} {op:?} {rhs_type:?}"
                            );
                        }
                        // Arithmetic on numerics; Plus on strings is
                        // concatenation (codegen already emits ConcatStr).
                        let is_string_plus =
                            matches!((&lhs_type, &op), (Type::String, Token::Plus));
                        match lhs_type {
                            Type::Integer | Type::Float => Some(lhs_type),
                            Type::String if is_string_plus => Some(Type::String),
                            _ => {
                                panic!(
                                    "arithmetic operation {op:?} not supported for type {lhs_type:?}"
                                );
                            }
                        }
                    }

                    // Equality and Comparison (Always returns Boolean)
                    Token::EqEq
                    | Token::BangEq
                    | Token::MoreThan
                    | Token::LessThan
                    | Token::MoreEq
                    | Token::LessEq => {
                        if type_eq(lhs_type.clone(), rhs_type.clone()).is_err() {
                            panic!(
                                "cannot compare mismatched types: {lhs_type:?} {op:?} {rhs_type:?}"
                            );
                        }
                        Some(Type::Bool)
                    }

                    // Logical Operations (Requires Booleans)
                    Token::AmpAmp | Token::PipePipe => {
                        if !matches!(lhs_type, Type::Bool) || !matches!(rhs_type, Type::Bool) {
                            panic!("logical operation {op:?} requires boolean operands");
                        }
                        Some(Type::Bool)
                    }

                    // Bitwise Operations (Usually requires Integers)
                    Token::Ampersand
                    | Token::Pipe
                    | Token::Caret
                    | Token::LShift
                    | Token::RShift => {
                        if !matches!(lhs_type, Type::Integer) || !matches!(rhs_type, Type::Integer)
                        {
                            panic!("bitwise operation {op:?} requires integer operands");
                        }
                        Some(Type::Integer)
                    }

                    Token::Eq => {
                        if type_eq(lhs_type.clone(), rhs_type.clone()).is_err() {
                            panic!(
                                "cannot assign mismatched types: {lhs_type:?} {op:?} {rhs_type:?}\n{lhs:?} = {rhs:?}"
                            );
                        }
                        Some(lhs_type)
                    }

                    _ => {
                        panic!("unsupported binary operator: {op:?}\n{lhs:?}\n{rhs:?}");
                    }
                }
            }
            Ast::ExprUnary { child, op } => {
                let child_type = self.check(child)?;

                match op {
                    Token::Dash => {
                        if type_eq(child_type.clone(), Type::Integer).is_err()
                            && type_eq(child_type.clone(), Type::Float).is_err()
                        {
                            panic!("cannot negate a non number type: {child:?}({child_type:?})");
                        } else {
                            Some(child_type)
                        }
                    }
                    Token::Bang => {
                        if type_eq(child_type.clone(), Type::Integer).is_err()
                            && type_eq(child_type.clone(), Type::Float).is_err()
                            && type_eq(child_type.clone(), Type::Bool).is_err()
                        {
                            panic!("can't not this type: {child:?}({child_type:?})");
                        } else {
                            Some(child_type)
                        }
                    }
                    Token::Tilde => {
                        if type_eq(child_type.clone(), Type::Integer).is_err() {
                            panic!("cannot bitwise invert a non integer type");
                        } else {
                            Some(child_type)
                        }
                    }
                    _ => {
                        panic!("unsupported unary operator: {op:?}\n{child:?}");
                    }
                }
            }
            Ast::ExprFuncCall { name, args } => {
                if name == "print" {
                    for arg in args {
                        self.check(arg)?;
                    }
                    return Some(Type::Void);
                }
                // Relix M6 extension: `remote_call(peer: str, method: str, arg: str) -> str`
                // is a built-in known to the codegen (emits Inst::RemoteCall). Validate
                // arity and arg types so a SOL author gets a real error message instead
                // of the generic "undefined function" panic.
                //
                // RELIX-2 step 4 adds the streaming variant
                // `remote_call_stream(peer, method, arg) -> str`. Same shape
                // as `remote_call` — codegen emits a different opcode but the
                // type / arity contract is identical.
                if name == "remote_call" || name == "remote_call_stream" {
                    if args.len() != 3 {
                        panic!(
                            "{name} expects 3 arguments (peer, method, arg) but received {}",
                            args.len()
                        );
                    }
                    for (i, arg) in args.iter_mut().enumerate() {
                        let arg_type = self.check(arg)?;
                        if type_eq(arg_type.clone(), Type::String).is_err() {
                            panic!(
                                "{name} expected str in position {i} but was passed {:?}",
                                arg_type
                            );
                        }
                    }
                    return Some(Type::String);
                }
                // F2 (SOL try/catch) built-ins. Three zero-arg
                // accessors expose the current error context
                // inside a catch block. Calling them outside a
                // catch returns the empty string / 0.
                if name == "error_kind" || name == "error_cause" {
                    if !args.is_empty() {
                        panic!("{name}() takes no arguments");
                    }
                    return Some(Type::String);
                }
                if name == "error_retry_hint" {
                    if !args.is_empty() {
                        panic!("error_retry_hint() takes no arguments");
                    }
                    return Some(Type::Integer);
                }
                // RELIX-7.19: confidence accessor. Zero-arg
                // builtin returning the score of the most
                // recently completed remote_call (1.0 before
                // any call).
                if name == "last_confidence" {
                    if !args.is_empty() {
                        panic!("last_confidence() takes no arguments");
                    }
                    return Some(Type::Float);
                }
                // F6: list built-ins. Element type tracking is
                // intentionally coarse — lists are
                // heterogeneous and operators use them as
                // string lists. Argument arity is validated;
                // argument types are not type-checked beyond
                // the obvious "you need a list here" sites.
                if name == "list_len" {
                    if args.len() != 1 {
                        panic!("list_len(lst) takes 1 argument, got {}", args.len());
                    }
                    for arg in args {
                        self.check(arg);
                    }
                    return Some(Type::Integer);
                }
                if name == "list_get" {
                    if args.len() != 2 {
                        panic!("list_get(lst, idx) takes 2 arguments, got {}", args.len());
                    }
                    for arg in args {
                        self.check(arg);
                    }
                    return Some(Type::String);
                }
                // F11: typed accessor — same shape as list_get
                // but returns a `Type::List` and the runtime
                // halts with `VM_ERROR_SENTINEL` when the
                // element is not a list. Operators reach for
                // this when they want to assert structure.
                if name == "list_get_list" {
                    if args.len() != 2 {
                        panic!(
                            "list_get_list(lst, idx) takes 2 arguments, got {}",
                            args.len()
                        );
                    }
                    for arg in args {
                        self.check(arg);
                    }
                    return Some(Type::List);
                }
                if name == "list_push" {
                    if args.len() != 2 {
                        panic!("list_push(lst, val) takes 2 arguments, got {}", args.len());
                    }
                    for arg in args {
                        self.check(arg);
                    }
                    return Some(Type::List);
                }
                if name == "list_contains" {
                    if args.len() != 2 {
                        panic!(
                            "list_contains(lst, val) takes 2 arguments, got {}",
                            args.len()
                        );
                    }
                    for arg in args {
                        self.check(arg);
                    }
                    return Some(Type::Bool);
                }
                if name == "list_join" {
                    if args.len() != 2 {
                        panic!("list_join(lst, sep) takes 2 arguments, got {}", args.len());
                    }
                    for arg in args {
                        self.check(arg);
                    }
                    return Some(Type::String);
                }
                if name == "list_split" {
                    if args.len() != 2 {
                        panic!("list_split(s, sep) takes 2 arguments, got {}", args.len());
                    }
                    for arg in args {
                        self.check(arg);
                    }
                    return Some(Type::List);
                }
                // F8: map built-ins. Same shape as list_*.
                if name == "map_get" {
                    if args.len() != 2 {
                        panic!("map_get(m, k) takes 2 arguments, got {}", args.len());
                    }
                    for arg in args {
                        self.check(arg);
                    }
                    return Some(Type::String);
                }
                // F11: typed accessor — same shape as map_get
                // but returns a `Type::Map` and the runtime
                // halts with `VM_ERROR_SENTINEL` when the
                // value is not a map.
                if name == "map_get_map" {
                    if args.len() != 2 {
                        panic!("map_get_map(m, k) takes 2 arguments, got {}", args.len());
                    }
                    for arg in args {
                        self.check(arg);
                    }
                    return Some(Type::Map);
                }
                if name == "map_set" {
                    if args.len() != 3 {
                        panic!("map_set(m, k, v) takes 3 arguments, got {}", args.len());
                    }
                    for arg in args {
                        self.check(arg);
                    }
                    return Some(Type::Map);
                }
                if name == "map_has" {
                    if args.len() != 2 {
                        panic!("map_has(m, k) takes 2 arguments, got {}", args.len());
                    }
                    for arg in args {
                        self.check(arg);
                    }
                    return Some(Type::Bool);
                }
                if name == "map_keys" {
                    if args.len() != 1 {
                        panic!("map_keys(m) takes 1 argument, got {}", args.len());
                    }
                    for arg in args {
                        self.check(arg);
                    }
                    return Some(Type::List);
                }
                if name == "map_len" {
                    if args.len() != 1 {
                        panic!("map_len(m) takes 1 argument, got {}", args.len());
                    }
                    for arg in args {
                        self.check(arg);
                    }
                    return Some(Type::Integer);
                }
                if name == "map_del" {
                    if args.len() != 2 {
                        panic!("map_del(m, k) takes 2 arguments, got {}", args.len());
                    }
                    for arg in args {
                        self.check(arg);
                    }
                    return Some(Type::Map);
                }
                // 1. Fetch and clone the signature in a temporary scope
                let (params, ret) = {
                    let entry = self.get_entry(&name).unwrap_or_else(|| {
                        panic!("attempting to make a function call on an undefined name `{name}`");
                    });

                    if let Symbol::Variable { kind } = entry
                        && let Type::Function { params, ret } = *kind.to_owned()
                    {
                        // Clone the params and ret to release the borrow on self
                        (params.clone(), ret.clone())
                    } else {
                        panic!(
                            "attempting to make a function call on a non-function type: `{name}`"
                        );
                    }
                }; // Borrow of self ends here

                // 2. Validate argument count
                if args.len() != params.len() {
                    panic!(
                        "function `{name}` expects {} arguments but received {}",
                        params.len(),
                        args.len()
                    );
                }

                // 3. Check each argument (safe to borrow self mutably now)
                for (i, (arg, param)) in args.iter_mut().zip(params.iter()).enumerate() {
                    let arg_type = self.check(arg)?;

                    if type_eq(arg_type.clone(), param.clone()).is_err() {
                        panic!(
                            "function `{name}` expected {:?} in position {i} but was passed {:?}",
                            param, arg_type
                        );
                    }
                }

                // 4. Return the return type
                Some(*ret)
            }
            Ast::ExprMemAcc { lhs, member } => {
                let lhs_type = self.check(lhs)?;
                let Type::Ident(sname) = lhs_type else {
                    panic!("{lhs_type:?} is not a struct with members");
                };

                let mem_type = {
                    let Some(entry) = self.get_entry(&sname) else {
                        panic!("could not find struct `{sname}` in scope");
                    };

                    let Symbol::Struct { fields } = entry else {
                        panic!("`{sname}` is not a struct");
                    };

                    let Some(mem) = fields.get(member) else {
                        panic!("`{sname}` has no member `{member}`");
                    };

                    mem.clone()
                };

                Some(*mem_type)
            }
            Ast::ExprEnumVar { name, var } => {
                let Some(entry) = self.get_entry(&name) else {
                    panic!("could not find struct `{name}` in scope");
                };

                let Symbol::Enum { variants } = entry else {
                    panic!("`{name}` is not an enum");
                };

                if variants.get(var).is_none() {
                    panic!("`{name}` has no variant `{var}`");
                };

                Some(Type::Ident(name.clone()))
            }
            Ast::ExprArrAcc { lhs, index } => {
                let lhs_type = self.check(lhs)?;
                let index_type = self.check(index)?;

                if !matches!(index_type, Type::Integer) && !matches!(index_type, Type::Float) {
                    panic!("Type Error: Array index must be an integer or float");
                }

                match lhs_type {
                    Type::Array { inner, .. } => Some(*inner),
                    _ => panic!("Type Error: Cannot index into a non-array type"),
                }
            }
            Ast::ExprReturn { val } => {
                if !self.can_return {
                    panic!("illegal return statement");
                }
                let actual = match val {
                    Some(v) => self.check(&mut *v)?,
                    None => Type::Void,
                };
                // Validate against the enclosing function's
                // declared return type. The check fires for
                // returns at any depth — top-level and
                // anything nested in if / else / while / for
                // / try / catch — because `return_type` stays
                // set for the whole DeclFunc body walk.
                if let Some(expected) = self.return_type.clone()
                    && type_eq(actual.clone(), expected.clone()).is_err()
                {
                    panic!(
                        "return type mismatch: function declared as returning `{expected:?}` \
                         but this return produces `{actual:?}`"
                    );
                }
                Some(actual)
            }
            Ast::ExprInteger(_) => Some(Type::Integer),
            Ast::ExprFloat(_) => Some(Type::Float),
            Ast::ExprString(_) => Some(Type::String),
            Ast::ExprChar(_) => Some(Type::Char),
            Ast::ExprBool(_) => Some(Type::Bool),
            Ast::ExprVar(name) => {
                let var_type = {
                    let entry = self.get_entry(&name).unwrap_or_else(|| {
                        panic!("variable `{name}` could not be found in the current scope");
                    });

                    if let Symbol::Variable { kind: var_type } = entry {
                        var_type.clone()
                    } else {
                        panic!("`{name}` is not a variable\n\t{entry:?}");
                    }
                };
                Some(*var_type)
            }
            // F5 / F7: literal forms type-check their children
            // for side-effect (catching e.g. an undefined
            // variable inside the literal) but produce a
            // nominal `Type::List` / `Type::Map` regardless of
            // the element / value shape — element types are
            // not tracked in the analyzer.
            Ast::ExprList { elements } => {
                for elem in elements.iter_mut() {
                    self.check(elem);
                }
                Some(Type::List)
            }
            Ast::ExprMap { pairs } => {
                for (_k, v) in pairs.iter_mut() {
                    self.check(v);
                }
                Some(Type::Map)
            }
            // Ast::ExprStructInit { name, fields } => {}
            x => todo!("{x:?}"),
        }
    }
}

/// Conservative return-coverage check for a function body.
/// Returns `true` when every static control-flow path through
/// `node` reaches a `return` (or diverging `rethrow`); `false`
/// when at least one path falls through.
///
/// The check fires only the cases the user can be sure of:
///
/// * A `Block` always-returns if any statement in it
///   always-returns (execution stops at the first return; any
///   later statements are dead).
/// * A `return` always-returns. A `rethrow` always-diverges,
///   which is equivalent for this analysis (the function
///   never falls through past it).
/// * An `if` with an `else` always-returns iff BOTH branches
///   always-return. An `if` without an `else` does not — the
///   `false` path falls through.
/// * A `try` always-returns iff the body returns AND every
///   catch body returns. This is a permissive approximation:
///   an uncaught failure leaves the function via the
///   error path, which is not a normal return, but for return
///   coverage we treat it the same as the catch path.
/// * `while` / `for` bodies do NOT count — the loop may
///   execute zero iterations, and we don't try to prove
///   otherwise.
/// * Anything else (lets, expression statements, calls,
///   prints, imports, struct / enum decls) does not count
///   as returning.
fn block_always_returns(node: &Ast) -> bool {
    match node {
        Ast::Block { block, .. } => block.iter().any(block_always_returns),
        Ast::ExprReturn { .. } => true,
        Ast::StmtRethrow => true,
        Ast::StmtIf {
            body,
            alt: Some(else_branch),
            ..
        } => block_always_returns(body) && block_always_returns(else_branch),
        Ast::StmtTry { body, catches } => {
            block_always_returns(body)
                && catches
                    .iter()
                    .all(|(_kind, catch_body)| block_always_returns(catch_body))
        }
        _ => false,
    }
}
