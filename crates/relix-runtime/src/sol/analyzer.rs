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
}

impl Analyzer {
    pub fn new() -> Self {
        Self {
            tt_arena: Vec::new(),
            tts: Vec::new(),
            can_break: false,
            can_return: false,
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
            eprintln!("\x1b[0;31merror\x1b[0;0m: redefinition of `{}`", name);
            std::process::exit(1);
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
                name: _,
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
                for (name, kind) in params {
                    self.add_entry(
                        name.to_owned(),
                        Symbol::Variable {
                            kind: Box::from(kind.clone()),
                        },
                    );
                }

                let old = self.can_return;
                self.can_return = true;
                // TODO: actually do branch checking please
                self.check(body);
                // let ret_type = match self.check(*body) {
                //     Some(ty) => ty,
                //     None => {
                //         eprintln!("function `{name}` has diverging types throughout its branches");
                //         std::process::exit(1);
                //     }
                // };
                // if type_eq(ret_type.clone(), ret.clone()).is_err() {
                //     eprintln!("return type of function `{name}` ({ret:?}) does not match its body ({ret_type:?})");
                //     std::process::exit(1);
                // }
                self.can_return = old;

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
                    eprintln!(
                        "condition of if statement must be of type `bool`, got {:?}",
                        cond
                    );
                    std::process::exit(1);
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
                    eprintln!(
                        "condition of if statement must be of type `bool`, got {:?}",
                        cond
                    );
                    std::process::exit(1);
                }
                let old = self.can_break;
                self.can_break = true;
                self.check(body);
                self.can_break = old;

                Some(Type::Void)
            }
            Ast::StmtFor {
                elem_name,
                array,
                body,
            } => {
                let Some(arr_type) = self.check(array) else {
                    eprintln!(
                        "array in which for loop is iterating over must have the known type `Array`"
                    );
                    std::process::exit(1);
                };
                let Type::Array { inner, .. } = arr_type else {
                    eprintln!(
                        "array in which for loop is iterating over must have the known type `Array`"
                    );
                    std::process::exit(1);
                };

                self.add_entry(elem_name.to_owned(), Symbol::Variable { kind: inner });
                let old = self.can_break;
                self.can_break = true;
                self.check(body);
                self.can_break = old;

                Some(Type::Void)
            }
            Ast::ExprAssign { var_name, value } => {
                let var_type = {
                    let entry = self.get_entry(&var_name).unwrap_or_else(|| {
                        eprintln!("variable `{var_name}` is assigned to before initialization");
                        std::process::exit(1);
                    });

                    if let Symbol::Variable { kind: var_type } = entry {
                        var_type.clone()
                    } else {
                        eprintln!(
                            "`{var_name}` is assigned to, however it is not a variable\n\t{entry:?}"
                        );
                        std::process::exit(1);
                    }
                };

                let rhs_type = self.check(value)?;
                if type_eq(*var_type.clone(), rhs_type.clone()).is_err() {
                    eprintln!("variable `{var_name}` is assigned to before initialization");
                    std::process::exit(1);
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
                            eprintln!(
                                "mismatched types in arithmetic: {lhs_type:?} {op:?} {rhs_type:?}"
                            );
                            std::process::exit(1);
                        }
                        // Arithmetic usually only works on numeric types
                        match lhs_type {
                            Type::Integer | Type::Float => Some(lhs_type),
                            _ => {
                                eprintln!(
                                    "arithmetic operation {op:?} not supported for type {lhs_type:?}"
                                );
                                std::process::exit(1);
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
                            eprintln!(
                                "cannot compare mismatched types: {lhs_type:?} {op:?} {rhs_type:?}"
                            );
                            std::process::exit(1);
                        }
                        Some(Type::Bool)
                    }

                    // Logical Operations (Requires Booleans)
                    Token::AmpAmp | Token::PipePipe => {
                        if !matches!(lhs_type, Type::Bool) || !matches!(rhs_type, Type::Bool) {
                            eprintln!("logical operation {op:?} requires boolean operands");
                            std::process::exit(1);
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
                            eprintln!("bitwise operation {op:?} requires integer operands");
                            std::process::exit(1);
                        }
                        Some(Type::Integer)
                    }

                    Token::Eq => {
                        if type_eq(lhs_type.clone(), rhs_type.clone()).is_err() {
                            eprintln!(
                                "cannot assign mismatched types: {lhs_type:?} {op:?} {rhs_type:?}\n{lhs:?} = {rhs:?}"
                            );
                            std::process::exit(1);
                        }
                        Some(lhs_type)
                    }

                    _ => {
                        eprintln!("unsupported binary operator: {op:?}\n{lhs:?}\n{rhs:?}");
                        std::process::exit(1);
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
                            eprintln!("cannot negate a non number type: {child:?}({child_type:?})");
                            std::process::exit(1);
                        } else {
                            Some(child_type)
                        }
                    }
                    Token::Bang => {
                        if type_eq(child_type.clone(), Type::Integer).is_err()
                            && type_eq(child_type.clone(), Type::Float).is_err()
                            && type_eq(child_type.clone(), Type::Bool).is_err()
                        {
                            eprintln!("can't not this type: {child:?}({child_type:?})");
                            std::process::exit(1);
                        } else {
                            Some(child_type)
                        }
                    }
                    Token::Tilde => {
                        if type_eq(child_type.clone(), Type::Integer).is_err() {
                            eprintln!("cannot bitwise invert a non integer type");
                            std::process::exit(1);
                        } else {
                            Some(child_type)
                        }
                    }
                    _ => {
                        eprintln!("unsupported unary operator: {op:?}\n{child:?}");
                        std::process::exit(1);
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
                if name == "remote_call" {
                    if args.len() != 3 {
                        eprintln!(
                            "remote_call expects 3 arguments (peer, method, arg) but received {}",
                            args.len()
                        );
                        std::process::exit(1);
                    }
                    for (i, arg) in args.iter_mut().enumerate() {
                        let arg_type = self.check(arg)?;
                        if type_eq(arg_type.clone(), Type::String).is_err() {
                            eprintln!(
                                "remote_call expected str in position {i} but was passed {:?}",
                                arg_type
                            );
                            std::process::exit(1);
                        }
                    }
                    return Some(Type::String);
                }
                // 1. Fetch and clone the signature in a temporary scope
                let (params, ret) = {
                    let entry = self.get_entry(&name).unwrap_or_else(|| {
                        eprintln!(
                            "attempting to make a function call on an undefined name `{name}`"
                        );
                        std::process::exit(1);
                    });

                    if let Symbol::Variable { kind } = entry
                        && let Type::Function { params, ret } = *kind.to_owned()
                    {
                        // Clone the params and ret to release the borrow on self
                        (params.clone(), ret.clone())
                    } else {
                        eprintln!(
                            "attempting to make a function call on a non-function type: `{name}`"
                        );
                        std::process::exit(1);
                    }
                }; // Borrow of self ends here

                // 2. Validate argument count
                if args.len() != params.len() {
                    eprintln!(
                        "function `{name}` expects {} arguments but received {}",
                        params.len(),
                        args.len()
                    );
                    std::process::exit(1);
                }

                // 3. Check each argument (safe to borrow self mutably now)
                for (i, (arg, param)) in args.iter_mut().zip(params.iter()).enumerate() {
                    let arg_type = self.check(arg)?;

                    if type_eq(arg_type.clone(), param.clone()).is_err() {
                        eprintln!(
                            "function `{name}` expected {:?} in position {i} but was passed {:?}",
                            param, arg_type
                        );
                        std::process::exit(1);
                    }
                }

                // 4. Return the return type
                Some(*ret)
            }
            Ast::ExprMemAcc { lhs, member } => {
                let lhs_type = self.check(lhs)?;
                let Type::Ident(sname) = lhs_type else {
                    eprintln!("{lhs_type:?} is not a struct with members");
                    std::process::exit(1);
                };

                let mem_type = {
                    let Some(entry) = self.get_entry(&sname) else {
                        eprintln!("could not find struct `{sname}` in scope");
                        std::process::exit(1);
                    };

                    let Symbol::Struct { fields } = entry else {
                        eprintln!("`{sname}` is not a struct");
                        std::process::exit(1);
                    };

                    let Some(mem) = fields.get(member) else {
                        eprintln!("`{sname}` has no member `{member}`");
                        std::process::exit(1);
                    };

                    mem.clone()
                };

                Some(*mem_type)
            }
            Ast::ExprEnumVar { name, var } => {
                let Some(entry) = self.get_entry(&name) else {
                    eprintln!("could not find struct `{name}` in scope");
                    std::process::exit(1);
                };

                let Symbol::Enum { variants } = entry else {
                    eprintln!("`{name}` is not an enum");
                    std::process::exit(1);
                };

                if variants.get(var).is_none() {
                    eprintln!("`{name}` has no variant `{var}`");
                    std::process::exit(1);
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
                    eprintln!("illegal return statement");
                    std::process::exit(1);
                }
                match val {
                    Some(v) => Some(self.check(&mut *v)?),
                    None => Some(Type::Void),
                }
            }
            Ast::ExprInteger(_) => Some(Type::Integer),
            Ast::ExprFloat(_) => Some(Type::Float),
            Ast::ExprString(_) => Some(Type::String),
            Ast::ExprChar(_) => Some(Type::Char),
            Ast::ExprBool(_) => Some(Type::Bool),
            Ast::ExprVar(name) => {
                let var_type = {
                    let entry = self.get_entry(&name).unwrap_or_else(|| {
                        eprintln!("variable `{name}` could not be found in the current scope");
                        std::process::exit(1);
                    });

                    if let Symbol::Variable { kind: var_type } = entry {
                        var_type.clone()
                    } else {
                        eprintln!("`{name}` is not a variable\n\t{entry:?}");
                        std::process::exit(1);
                    }
                };
                Some(*var_type)
            }
            // Ast::ExprStructInit { name, fields } => {}
            x => todo!("{x:?}"),
        }
    }
}
