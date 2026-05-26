use std::collections::HashMap;

use crate::sol::{
    analyzer::TypeTableId,
    lexer::{Token, TokenKind},
};

#[derive(Debug, Clone)]
pub enum Type {
    Void,
    Integer,
    Float,
    String,
    Char,
    Bool,

    Tuple(Vec<Type>),
    Array {
        size: Option<i128>,
        inner: Box<Type>,
    },
    /// F5: heterogeneous list, `let xs: list = [a, b, c];`.
    /// Element values are stored as raw heap refs at the VM
    /// level; in practice operators use them as string lists.
    /// Built-ins (`list_get`, `list_join`, …) treat elements
    /// as strings.
    List,
    /// F7: string-keyed map, `let m: map = { "k": v, … };`.
    /// Values are raw heap refs; built-ins treat them as
    /// strings the same way `list_*` does.
    Map,
    Ident(String),
    Function {
        params: Vec<Type>,
        ret: Box<Type>,
    },
}

pub type Program = Vec<Ast>;
#[derive(Debug, Clone)]
pub enum Ast {
    DeclFunc {
        name: String,
        params: Vec<(String, Type)>,
        ret: Type,
        body: Box<Ast>,
        scope: TypeTableId,
    },
    DeclVar {
        name: String,
        kind: Type,
        value: Option<Box<Ast>>,
    },
    DeclStruct {
        name: String,
        fields: HashMap<String, Type>,
    },
    DeclEnum {
        name: String,
        variants: HashMap<String, isize>,
    },

    Block {
        block: Vec<Ast>,
        scope: TypeTableId,
    },
    StmtImport {
        #[allow(dead_code)]
        path: Vec<String>,
        alias: Option<String>,
    },
    StmtIf {
        condition: Box<Ast>,
        body: Box<Ast>,
        alt: Option<Box<Ast>>,
    },
    StmtWhile {
        condition: Box<Ast>,
        body: Box<Ast>,
    },
    StmtFor {
        elem_name: String,
        array: Box<Ast>,
        body: Box<Ast>,
    },
    /// `try { body } catch <kind> { body } [catch <kind> { body }]*`
    /// — error recovery for `remote_call` failures. Each catch
    /// clause's `kind` is one of the documented classifications:
    /// `any`, `timeout`, `mesh_error`, `policy_denied`,
    /// `responder_error`. The body runs when the try-block fails
    /// with a matching kind; `any` matches every failure. Inside
    /// a catch body, the built-ins `error_kind()`,
    /// `error_cause()`, and `error_retry_hint()` return the
    /// current error's fields. `rethrow` re-raises so an outer
    /// try-block can handle the same failure.
    StmtTry {
        body: Box<Ast>,
        catches: Vec<(String, Ast)>,
    },
    /// `rethrow;` inside a catch — propagates the current error
    /// to the next outer try-handler or halts the VM if none.
    StmtRethrow,

    #[allow(dead_code)]
    ExprAssign {
        var_name: String,
        value: Box<Ast>,
    },
    ExprBinary {
        lhs: Box<Ast>,
        rhs: Box<Ast>,
        op: Token,
    },
    ExprUnary {
        child: Box<Ast>,
        op: Token,
    },
    ExprFuncCall {
        name: String,
        args: Vec<Ast>,
    },
    ExprMemAcc {
        lhs: Box<Ast>,
        member: String,
    },
    ExprEnumVar {
        name: String,
        var: String,
    },
    ExprArrAcc {
        lhs: Box<Ast>,
        index: Box<Ast>,
    },
    ExprReturn {
        val: Option<Box<Ast>>,
    },
    ExprInteger(i128),
    ExprFloat(f64),
    ExprString(String),
    ExprChar(char),
    ExprBool(bool),
    ExprUndefined,
    ExprVar(String),
    ExprStructInit {
        name: String,
        fields: Vec<(String, Ast)>,
    },
    ExprArrayInit {
        values: Vec<Ast>,
    },
    /// F5: SOL list literal `[a, b, c]`. Empty `[]` is valid.
    /// Elements compile to a sequence of PushConst / variable
    /// pushes followed by `Inst::PushList(n)`. Unlike
    /// `ExprArrayInit` (typed, fixed-size) lists are
    /// heterogeneous at the VM level and grow dynamically.
    ExprList {
        elements: Vec<Ast>,
    },
    /// F7: SOL map literal `{ "k1": v1, "k2": v2, ... }`.
    /// Keys MUST be string literals; values are any
    /// expression. Empty `{}` is valid in an expression
    /// position. Compiles to alternating `key, value` pushes
    /// followed by `Inst::PushMap(n)` where `n` is the pair
    /// count.
    ExprMap {
        pairs: Vec<(String, Ast)>,
    },
}

/// Expand `{{name}}` markers inside a string literal into a
/// concatenation of `ExprString` + `ExprVar` chunks joined by
/// `Token::Plus`. The result is identical to writing
/// `"prefix " + name + " suffix"` by hand — no new VM opcode
/// or runtime path is needed.
///
/// Rules:
///
/// - `{{ident}}` becomes `ExprVar(ident)` where `ident` is
///   one or more `[A-Za-z0-9_]` chars. Whitespace inside the
///   braces is trimmed.
/// - `{{` without a closing `}}` is preserved verbatim so the
///   operator sees the typo in their flow source.
/// - `{{}}` (empty marker) is preserved verbatim for the same
///   reason.
/// - A string with no `{{...}}` markers returns the original
///   `Ast::ExprString(s)` with zero allocation overhead.
pub fn expand_string_interpolation(raw: &str) -> Ast {
    if !raw.contains("{{") {
        return Ast::ExprString(raw.to_string());
    }
    let mut chunks: Vec<Ast> = Vec::new();
    let mut buf = String::new();
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        // Look ahead for `{{` opener.
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find the matching `}}`.
            let body_start = i + 2;
            let mut j = body_start;
            let mut closer: Option<usize> = None;
            while j + 1 < bytes.len() {
                if bytes[j] == b'}' && bytes[j + 1] == b'}' {
                    closer = Some(j);
                    break;
                }
                j += 1;
            }
            match closer {
                Some(end) => {
                    let name = std::str::from_utf8(&bytes[body_start..end])
                        .unwrap_or("")
                        .trim();
                    let is_ident =
                        !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_');
                    if !is_ident {
                        // Empty or non-identifier marker: keep
                        // the original `{{...}}` text so the
                        // operator sees what went wrong.
                        let raw_marker = std::str::from_utf8(&bytes[i..end + 2]).unwrap_or("");
                        buf.push_str(raw_marker);
                        i = end + 2;
                        continue;
                    }
                    // Flush buffered literal text.
                    if !buf.is_empty() {
                        chunks.push(Ast::ExprString(std::mem::take(&mut buf)));
                    }
                    chunks.push(Ast::ExprVar(name.to_string()));
                    i = end + 2;
                    continue;
                }
                None => {
                    // Unterminated `{{` — preserve verbatim.
                    let tail = std::str::from_utf8(&bytes[i..]).unwrap_or("");
                    buf.push_str(tail);
                    i = bytes.len();
                    continue;
                }
            }
        }
        // Push one UTF-8 char into the literal buffer.
        let ch = raw[i..].chars().next().unwrap();
        let n = ch.len_utf8();
        buf.push(ch);
        i += n;
    }
    if !buf.is_empty() {
        chunks.push(Ast::ExprString(buf));
    }
    if chunks.is_empty() {
        return Ast::ExprString(String::new());
    }
    // Fold left into a concat chain: `a + b + c`.
    let mut iter = chunks.into_iter();
    let mut acc = iter.next().unwrap();
    for next in iter {
        acc = Ast::ExprBinary {
            lhs: Box::new(acc),
            rhs: Box::new(next),
            op: Token::Plus,
        };
    }
    acc
}

/// Build a left-folded concat chain (`a + b + c + ...`) from
/// the given chunks. Used by the `delegate` / `send` sugar
/// lowerings to assemble the pipe-separated wire payload that
/// the coordinator's `delegate.spawn` / `msg.send` capabilities
/// expect, without introducing any new VM machinery. Panics
/// when called with zero chunks (caller invariant).
fn concat_chain(chunks: Vec<Ast>) -> Ast {
    let mut iter = chunks.into_iter();
    let mut acc = iter.next().expect("concat_chain: at least one chunk");
    for next in iter {
        acc = Ast::ExprBinary {
            lhs: Box::new(acc),
            rhs: Box::new(next),
            op: Token::Plus,
        };
    }
    acc
}

pub struct Parser {
    tokens: Vec<Token>,
    index: usize,
    can_struct: bool,
}

macro_rules! noob {
    ($self: expr) => {
        $self.index < $self.tokens.len()
    };
}

impl Parser {
    pub fn from(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            index: 0,
            can_struct: true,
        }
    }

    fn eat(&mut self, tk: TokenKind, msg: &str) {
        let tkcurr = self.tokens[self.index].get_kind();
        if tkcurr != tk {
            self.debtok(4);
            panic!("{}", msg);
        }
        self.index += 1;
    }
    fn current(&self) -> Token {
        self.tokens[self.index].clone()
    }
    fn advance(&mut self) -> Token {
        self.index += 1;
        self.tokens[self.index - 1].clone()
    }

    fn debtok(&self, radius: usize) {
        let r = radius as isize;
        for xoff in -r..=r {
            if self.index as isize + xoff < 0 {
                continue;
            }
            eprintln!(
                "{} {:?}",
                if xoff == 0 { '>' } else { ' ' },
                self.tokens[(self.index as isize + xoff) as usize]
            );
        }
    }

    pub fn run(&mut self) -> Program {
        std::iter::from_fn(|| self.declaration()).collect()
    }
    fn declaration(&mut self) -> Option<Ast> {
        if self.index >= self.tokens.len() {
            return None;
        }
        match self.current().clone() {
            Token::Func => self.func_decl(),
            Token::Let => self.var_decl(),
            Token::Struct => self.struct_decl(),
            Token::Enum => self.enum_decl(),
            Token::Import => self.import_stmt(),
            x => {
                self.debtok(4);
                panic!("unknown declaration: {x:?}")
            }
        }
    }

    fn parse_type(&mut self) -> Option<Type> {
        match self.tokens[self.index].clone() {
            Token::Ident(ptype) => {
                let ty = match ptype.as_str() {
                    "int" => Some(Type::Integer),
                    "float" => Some(Type::Float),
                    "str" => Some(Type::String),
                    "char" => Some(Type::Char),
                    "bool" => Some(Type::Bool),
                    // F5 / F7: typed `list` and `map` declarations.
                    // Element / value types are not tracked — both
                    // are heterogeneous at the VM level and the
                    // built-ins treat values as strings.
                    "list" => Some(Type::List),
                    "map" => Some(Type::Map),
                    _ => Some(Type::Ident(ptype)),
                };
                self.index += 1;
                ty
            }
            Token::LSquare => {
                self.index += 1;

                let size = if self.tokens[self.index].get_kind() != TokenKind::RSquare {
                    let Token::Integer(s) = self.tokens[self.index].clone() else {
                        panic!("only integers can be used to specify an array size");
                    };
                    self.index += 1;
                    Some(s)
                } else {
                    None
                };
                self.eat(TokenKind::RSquare, "expected `]` after array size");

                let inner = Box::new(self.parse_type()?);

                Some(Type::Array { size, inner })
            }
            Token::LParen => {
                self.index += 1;

                let mut types = Vec::new();
                while noob!(self) && !matches!(self.current(), Token::RParen) {
                    types.push(self.parse_type()?);
                    if matches!(self.current(), Token::Comma) {
                        self.index += 1;
                    } else {
                        break;
                    }
                }
                self.eat(TokenKind::RParen, "expected an `)` after tuple type");

                Some(Type::Tuple(types))
            }
            x => {
                self.debtok(4);
                panic!("`{:?}` is not valid in a type specifier", x);
            }
        }
    }

    fn func_decl(&mut self) -> Option<Ast> {
        self.index += 1;
        let Token::Ident(name) = self.tokens[self.index].clone() else {
            panic!("name expected after function keyword");
        };
        self.index += 1;
        self.eat(
            TokenKind::LParen,
            "expected left parenthesis after function name",
        );

        let mut params = Vec::new();
        while noob!(self) && !matches!(self.tokens[self.index], Token::RParen) {
            let Token::Ident(pname) = self.tokens[self.index].clone() else {
                self.debtok(4);

                panic!("expected parameter name");
            };
            self.index += 1;
            self.eat(TokenKind::Colon, "expected colon after parameter name");
            let ptype = self.parse_type()?;
            params.push((pname, ptype));
            if self.tokens[self.index].get_kind() == TokenKind::Comma {
                self.index += 1;
            } else {
                break;
            }
        }
        self.eat(
            TokenKind::RParen,
            "expected right parenthesis after parameter list",
        );

        let ret = if self.tokens[self.index].get_kind() == TokenKind::Arrow {
            self.index += 1;
            self.parse_type()?
        } else {
            Type::Void
        };

        let body = Box::new(self.block()?);

        Some(Ast::DeclFunc {
            name,
            params,
            ret,
            body,
            scope: usize::MAX,
        })
    }
    fn var_decl(&mut self) -> Option<Ast> {
        self.index += 1;

        let Token::Ident(name) = self.advance().clone() else {
            panic!("name expected after function keyword");
        };

        self.eat(
            TokenKind::Colon,
            "expected colon after variable name in a declaration",
        );
        let kind = self.parse_type()?;

        let value = if matches!(self.current(), Token::Eq) {
            self.advance();
            Some(Box::new(self.expression()?))
        } else {
            None
        };

        self.eat(
            TokenKind::Semi,
            "expected semicolon at the end of a variable declaration",
        );

        Some(Ast::DeclVar { name, kind, value })
    }

    fn block(&mut self) -> Option<Ast> {
        match self.tokens[self.index] {
            Token::LCurly => {
                self.index += 1;
                let mut stmts = Vec::new();
                while noob!(self) && !matches!(self.tokens[self.index], Token::RCurly) {
                    stmts.push(self.statement()?);
                }
                self.eat(TokenKind::RCurly, "left curly brace is never closed");
                Some(Ast::Block {
                    block: stmts,
                    scope: usize::MAX,
                }) // scope isn't filled in until analysis
            }
            _ => self.statement(),
        }
    }
    fn statement(&mut self) -> Option<Ast> {
        match self.tokens[self.index].clone() {
            Token::For => self.for_stmt(),
            Token::If => self.if_stmt(),
            Token::Import => self.import_stmt(),
            Token::While => self.while_stmt(),
            Token::Let => self.var_decl(),
            Token::Return => self.return_stmt(),
            Token::Try => self.try_stmt(),
            Token::Rethrow => {
                self.index += 1;
                self.eat(TokenKind::Semi, "expected `;` after `rethrow`");
                Some(Ast::StmtRethrow)
            }
            Token::LCurly => self.block(),
            x => {
                let expr = self.expression();
                if expr.is_some() {
                    self.eat(TokenKind::Semi, "expected semicolon to follow exprstmt");
                    expr
                } else {
                    self.debtok(4);

                    panic!(
                        "identifier `{:?}` is not the start of any known statement",
                        x
                    );
                }
            }
        }
    }
    fn for_stmt(&mut self) -> Option<Ast> {
        self.index += 1;

        let Token::Ident(elem_name) = self.tokens[self.index].clone() else {
            panic!("variable name expected after `for` keyword");
        };
        self.index += 1;

        self.eat(
            TokenKind::In,
            "expected `in` keyword to follow in a for declaration",
        );

        let old = self.can_struct;
        self.can_struct = false;
        let array = Box::new(self.expression()?);
        self.can_struct = old;

        self.eat(TokenKind::LCurly, "expected `{` after for loop declaration");
        self.index -= 1;
        let body = Box::new(self.block()?);

        Some(Ast::StmtFor {
            elem_name,
            array,
            body,
        })
    }
    /// Parse `try { body } catch <kind> { body } [catch ...]*`.
    /// At least one catch clause is required. Recognised kinds:
    /// `any`, `timeout`, `mesh_error`, `policy_denied`,
    /// `responder_error`. Other identifiers parse but won't
    /// match any classified error.
    fn try_stmt(&mut self) -> Option<Ast> {
        self.index += 1;
        self.eat(TokenKind::LCurly, "expected `{` after `try`");
        self.index -= 1;
        let body = Box::new(self.block()?);
        let mut catches: Vec<(String, Ast)> = Vec::new();
        while matches!(self.tokens.get(self.index), Some(Token::Catch)) {
            self.index += 1;
            let kind = match self.tokens[self.index].clone() {
                Token::Ident(s) => s,
                other => {
                    self.debtok(4);
                    panic!("expected catch kind identifier, got {other:?}");
                }
            };
            self.index += 1;
            self.eat(TokenKind::LCurly, "expected `{` after catch kind");
            self.index -= 1;
            let catch_body = self.block()?;
            catches.push((kind, catch_body));
        }
        if catches.is_empty() {
            self.debtok(4);
            panic!("`try` block requires at least one `catch <kind> {{ ... }}` clause");
        }
        Some(Ast::StmtTry { body, catches })
    }

    fn if_stmt(&mut self) -> Option<Ast> {
        self.index += 1;

        let old = self.can_struct;
        self.can_struct = false;
        let condition = Box::new(self.expression()?);
        self.can_struct = old;

        // eprintln!("{condition:#?}");
        self.eat(
            TokenKind::LCurly,
            "expected `{` after if statement declaration",
        );
        self.index -= 1;
        let body = Box::new(self.block()?);

        let alt = if matches!(self.tokens[self.index], Token::Else) {
            self.index += 1;
            Some(Box::new(self.block()?))
        } else {
            None
        };

        Some(Ast::StmtIf {
            condition,
            body,
            alt,
        })
    }
    fn while_stmt(&mut self) -> Option<Ast> {
        self.index += 1;

        let old = self.can_struct;
        self.can_struct = false;
        let condition = Box::new(self.expression()?);
        self.can_struct = old;

        self.eat(
            TokenKind::LCurly,
            "expected `{` after while loop declaration",
        );
        self.index -= 1;
        let body = Box::new(self.block()?);

        Some(Ast::StmtWhile { condition, body })
    }

    fn import_stmt(&mut self) -> Option<Ast> {
        self.index += 1;

        let mut path = Vec::new();
        {
            let Token::Ident(root) = self.tokens[self.index].clone() else {
                panic!("expected an identifier in an import path");
            };
            self.index += 1;
            path.push(root);
        }
        while noob!(self) && self.tokens[self.index].get_kind() == TokenKind::Dot {
            self.index += 1;
            let Token::Ident(section) = self.tokens[self.index].clone() else {
                panic!("expected an identifier in an import path");
            };
            self.index += 1;
            path.push(section);
        }

        let alias = if self.tokens[self.index].get_kind() == TokenKind::As {
            self.index += 1;
            let Token::Ident(section) = self.tokens[self.index].clone() else {
                panic!("expected an identifier for import to alias as");
            };
            self.index += 1;
            Some(section)
        } else {
            None
        };

        self.eat(
            TokenKind::Semi,
            "expected semicolon at the end of an import statement",
        );
        Some(Ast::StmtImport { path, alias })
    }
    fn return_stmt(&mut self) -> Option<Ast> {
        self.index += 1;

        let val = if matches!(self.current(), Token::Semi) {
            None
        } else {
            Some(Box::new(self.expression()?))
        };
        self.eat(
            TokenKind::Semi,
            "expected semicolon at the end of a return statement",
        );

        Some(Ast::ExprReturn { val })
    }
    fn struct_decl(&mut self) -> Option<Ast> {
        self.index += 1;

        let Token::Ident(name) = self.tokens[self.index].clone() else {
            panic!("expected a name after keyword `struct`");
        };
        self.index += 1;

        self.eat(TokenKind::LCurly, "expected `{` after enum declaration");

        let mut fields = HashMap::new();
        while noob!(self) && self.tokens[self.index].get_kind() != TokenKind::RCurly {
            let Token::Ident(fname) = self.tokens[self.index].clone() else {
                panic!("expected identifier for a field name in struct declaration");
            };
            self.index += 1;

            self.eat(TokenKind::Colon, "expected colon after field name");
            let fkind = self.parse_type()?;

            fields.insert(fname, fkind);
            if self.tokens[self.index].get_kind() == TokenKind::Comma {
                self.index += 1;
            } else {
                break;
            }
        }
        self.eat(
            TokenKind::RCurly,
            "expected `}` to close struct declaration",
        );

        Some(Ast::DeclStruct { name, fields })
    }
    fn enum_decl(&mut self) -> Option<Ast> {
        self.index += 1;

        let Token::Ident(name) = self.tokens[self.index].clone() else {
            panic!("expected a name after keyword `enum`");
        };
        self.index += 1;

        self.eat(TokenKind::LCurly, "expected `{` after enum declaration");

        let mut variants = HashMap::new();
        let mut iota = 0;
        while noob!(self) && self.tokens[self.index].get_kind() != TokenKind::RCurly {
            let Token::Ident(vname) = self.tokens[self.index].clone() else {
                panic!("expected identifier for a member name in enum declaration");
            };
            self.index += 1;

            if self.tokens[self.index].get_kind() == TokenKind::Eq {
                self.index += 1;
                let Token::Integer(viota) = self.tokens[self.index].clone() else {
                    panic!("expected an integer after equals sign in enum declaration");
                };
                self.index += 1;

                iota = viota as isize
            }

            variants.insert(vname, iota);
            iota += 1;
            if self.tokens[self.index].get_kind() == TokenKind::Comma {
                self.index += 1;
            } else {
                break;
            }
        }
        self.eat(TokenKind::RCurly, "expected `}` to close enum declaration");

        Some(Ast::DeclEnum { name, variants })
    }

    /// Consume the next token if it is `Token::Ident(kw)`,
    /// otherwise panic with `msg`. Used by the `delegate` and
    /// `send` sugar forms to parse contextual sub-keywords
    /// (`goal`, `from`, `to`, `subject`, `body`) without
    /// promoting them to real keywords — they remain valid
    /// identifiers everywhere else.
    fn eat_kw(&mut self, kw: &str, msg: &str) {
        let matched = matches!(&self.tokens[self.index], Token::Ident(s) if s == kw);
        if !matched {
            self.debtok(4);
            panic!("{msg}");
        }
        self.index += 1;
    }

    /// Peek at the current token and return true iff it is
    /// `Token::Ident(kw)`. Used by `primary()` to decide
    /// whether `delegate` / `send` should be treated as their
    /// sugar form or as a plain identifier.
    fn peek_kw(&self, kw: &str) -> bool {
        matches!(&self.tokens[self.index], Token::Ident(s) if s == kw)
    }

    /// Parse `delegate goal <goal_expr> from <parent_expr> to
    /// <target_expr>` and lower it to a synthetic call to the
    /// `remote_call("coord", "delegate.spawn", ...)` builtin.
    /// The lowering builds the pipe-separated payload that the
    /// coordinator's spawn handler expects:
    /// `parent_task_id|goal|context|target_subject_id|depth`.
    /// `context` is left empty and `depth` is hardcoded to `0`
    /// — power users who need either field call `remote_call`
    /// directly. The result has type `str` (the child task id)
    /// because that is what the underlying capability returns.
    fn delegate_sugar(&mut self) -> Ast {
        // Caller has already consumed the `delegate` ident.
        self.eat_kw("goal", "expected `goal` after `delegate`");
        let old_can_struct = self.can_struct;
        self.can_struct = false;
        let goal_expr = self
            .expression()
            .expect("expected goal expression in `delegate goal ...`");
        self.eat_kw(
            "from",
            "expected `from <parent_task_id>` after delegate goal",
        );
        let parent_expr = self
            .expression()
            .expect("expected parent_task_id expression in `delegate ... from ...`");
        self.eat_kw(
            "to",
            "expected `to <target_subject_id>` after delegate parent",
        );
        let target_expr = self
            .expression()
            .expect("expected target_subject_id expression in `delegate ... to ...`");
        self.can_struct = old_can_struct;

        // Assemble parent | goal | <empty context> | target | 0.
        let arg = concat_chain(vec![
            parent_expr,
            Ast::ExprString("|".to_string()),
            goal_expr,
            Ast::ExprString("||".to_string()),
            target_expr,
            Ast::ExprString("|0".to_string()),
        ]);
        Ast::ExprFuncCall {
            name: "remote_call".to_string(),
            args: vec![
                Ast::ExprString("coord".to_string()),
                Ast::ExprString("delegate.spawn".to_string()),
                arg,
            ],
        }
    }

    /// Parse `send subject <subj_expr> body <body_expr> from
    /// <from_expr> to <to_expr>` and lower it to a synthetic
    /// call to `remote_call("coord", "msg.send", ...)`. The
    /// lowering builds the pipe-separated payload:
    /// `from|to|subject|body|thread_id|reply_to|ttl_secs|origin_surface`.
    /// Optional fields (`thread_id`, `reply_to`, `ttl_secs`)
    /// default to empty / `0`. The `origin_surface` is
    /// hardcoded to `sol_flow` so the message store records
    /// where it came from.
    fn send_sugar(&mut self) -> Ast {
        // Caller has already consumed the `send` ident.
        self.eat_kw("subject", "expected `subject` after `send`");
        let old_can_struct = self.can_struct;
        self.can_struct = false;
        let subject_expr = self
            .expression()
            .expect("expected subject expression in `send subject ...`");
        self.eat_kw("body", "expected `body <body>` after send subject");
        let body_expr = self
            .expression()
            .expect("expected body expression in `send ... body ...`");
        self.eat_kw("from", "expected `from <from>` after send body");
        let from_expr = self
            .expression()
            .expect("expected from expression in `send ... from ...`");
        self.eat_kw("to", "expected `to <to>` after send from");
        let to_expr = self
            .expression()
            .expect("expected to expression in `send ... to ...`");
        self.can_struct = old_can_struct;

        // Assemble from | to | subject | body | <empty thread_id>
        // | <empty reply_to> | 0 ttl_secs | sol_flow origin.
        let arg = concat_chain(vec![
            from_expr,
            Ast::ExprString("|".to_string()),
            to_expr,
            Ast::ExprString("|".to_string()),
            subject_expr,
            Ast::ExprString("|".to_string()),
            body_expr,
            Ast::ExprString("|||0|sol_flow".to_string()),
        ]);
        Ast::ExprFuncCall {
            name: "remote_call".to_string(),
            args: vec![
                Ast::ExprString("coord".to_string()),
                Ast::ExprString("msg.send".to_string()),
                arg,
            ],
        }
    }

    fn left_rec(
        &mut self,
        symbols: &[TokenKind],
        child: fn(&mut Parser) -> Option<Ast>,
    ) -> Option<Ast> {
        let mut lhs = child(self)?;

        while symbols.contains(&self.current().get_kind()) {
            let op = self.advance();
            let rhs = child(self)?;
            lhs = Ast::ExprBinary {
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                op,
            }
        }

        Some(lhs)
    }
    fn right_rec(
        &mut self,
        symbols: &[TokenKind],
        parent: fn(&mut Parser) -> Option<Ast>,
        child: fn(&mut Parser) -> Option<Ast>,
    ) -> Option<Ast> {
        let lhs = parent(self)?;

        Some(if symbols.contains(&self.current().get_kind()) {
            let op = self.advance();
            let rhs = child(self)?;
            Ast::ExprBinary {
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                op: op,
            }
        } else {
            lhs
        })
    }
    fn expression(&mut self) -> Option<Ast> {
        self.assignment()
    }
    fn assignment(&mut self) -> Option<Ast> {
        self.right_rec(&[TokenKind::Eq], Self::logic_or, Self::assignment)
    }
    fn logic_or(&mut self) -> Option<Ast> {
        self.left_rec(&[TokenKind::PipePipe], Self::logic_and)
    }
    fn logic_and(&mut self) -> Option<Ast> {
        self.left_rec(&[TokenKind::AmpAmp], Self::bitwise_or)
    }
    fn bitwise_or(&mut self) -> Option<Ast> {
        self.left_rec(&[TokenKind::Pipe], Self::bitwise_xor)
    }
    fn bitwise_xor(&mut self) -> Option<Ast> {
        self.left_rec(&[TokenKind::Caret], Self::bitwise_and)
    }
    fn bitwise_and(&mut self) -> Option<Ast> {
        self.left_rec(&[TokenKind::Ampersand], Self::equality)
    }
    fn equality(&mut self) -> Option<Ast> {
        self.left_rec(&[TokenKind::EqEq, TokenKind::BangEq], Self::relational)
    }
    fn relational(&mut self) -> Option<Ast> {
        self.left_rec(
            &[
                TokenKind::LessThan,
                TokenKind::LessEq,
                TokenKind::MoreThan,
                TokenKind::MoreEq,
            ],
            Self::shift,
        )
    }
    fn shift(&mut self) -> Option<Ast> {
        self.left_rec(&[TokenKind::LShift, TokenKind::RShift], Self::additive)
    }
    fn additive(&mut self) -> Option<Ast> {
        self.left_rec(&[TokenKind::Plus, TokenKind::Dash], Self::multiplicative)
    }
    fn multiplicative(&mut self) -> Option<Ast> {
        self.left_rec(&[TokenKind::Star, TokenKind::Slash], Self::unary)
    }
    fn unary(&mut self) -> Option<Ast> {
        if [TokenKind::Bang, TokenKind::Dash, TokenKind::Tilde].contains(&self.current().get_kind())
        {
            let op = self.advance();
            Some(Ast::ExprUnary {
                child: Box::new(self.unary()?),
                op,
            })
        } else {
            self.postfix()
        }
    }
    fn postfix(&mut self) -> Option<Ast> {
        let mut lhs = self.primary()?;

        while let ck = self.current().get_kind()
            && (ck == TokenKind::Dot || ck == TokenKind::LSquare)
        {
            match ck {
                TokenKind::Dot => {
                    self.advance();
                    let Token::Ident(rhs) = self.advance() else {
                        panic!("`{:?}` is not a valid member", self.tokens[self.index - 1]);
                    };
                    lhs = Ast::ExprMemAcc {
                        lhs: Box::new(lhs),
                        member: rhs,
                    };
                }
                TokenKind::LSquare => {
                    self.advance();
                    let index = self.expression()?;
                    self.eat(TokenKind::RSquare, "expected ']' to close array index");
                    lhs = Ast::ExprArrAcc {
                        lhs: Box::new(lhs),
                        index: Box::new(index),
                    };
                }
                _ => unreachable!(),
            }
        }

        Some(lhs)
    }
    fn primary(&mut self) -> Option<Ast> {
        let kind = self.current().get_kind();

        let res = match kind {
            TokenKind::Integer => {
                if let Token::Integer(v) = self.advance() {
                    Some(Ast::ExprInteger(v))
                } else {
                    None
                }
            }
            TokenKind::Float => {
                if let Token::Float(v) = self.advance() {
                    Some(Ast::ExprFloat(v))
                } else {
                    None
                }
            }
            TokenKind::String => {
                if let Token::String(v) = self.advance() {
                    Some(expand_string_interpolation(&v))
                } else {
                    None
                }
            }
            TokenKind::Char => {
                if let Token::Char(v) = self.advance() {
                    Some(Ast::ExprChar(v))
                } else {
                    None
                }
            }
            TokenKind::True => {
                self.advance();
                Some(Ast::ExprBool(true))
            }
            TokenKind::False => {
                self.advance();
                Some(Ast::ExprBool(false))
            }
            TokenKind::Ident => {
                // Extract the name from the Token::Ident
                let name = if let Token::Ident(n) = self.advance() {
                    n
                } else {
                    unreachable!()
                };

                // Soft-keyword sugar: `delegate goal ...` and
                // `send subject ...` lower to remote_call. Both
                // require their first sub-keyword to disambiguate
                // from a plain variable named `delegate` or `send`.
                if name == "delegate" && self.peek_kw("goal") {
                    return Some(self.delegate_sugar());
                }
                if name == "send" && self.peek_kw("subject") {
                    return Some(self.send_sugar());
                }

                let next_kind = self.current().get_kind();

                if next_kind == TokenKind::LParen {
                    self.eat(TokenKind::LParen, "Expected '(' for function call");
                    let mut args = Vec::new();
                    if self.current().get_kind() != TokenKind::RParen {
                        loop {
                            args.push(self.expression()?);
                            if self.current().get_kind() == TokenKind::Comma {
                                self.advance();
                            } else {
                                break;
                            }
                        }
                    }
                    self.eat(TokenKind::RParen, "Expected ')' after arguments");
                    Some(Ast::ExprFuncCall { name, args })
                } else if self.can_struct && next_kind == TokenKind::LCurly {
                    self.eat(TokenKind::LCurly, "Expected '{' for struct initialization");
                    let mut fields = Vec::new();
                    while self.current().get_kind() != TokenKind::RCurly {
                        let field_token = self.advance();
                        if let Token::Ident(field_name) = field_token {
                            self.eat(TokenKind::Colon, "Expected ':' after field name");
                            let value = self.expression()?;
                            fields.push((field_name, value));
                            if self.current().get_kind() == TokenKind::Comma {
                                self.advance();
                            }
                        }
                    }
                    self.eat(TokenKind::RCurly, "Expected '}' after struct fields");
                    Some(Ast::ExprStructInit { name, fields })
                } else if next_kind == TokenKind::ColonColon {
                    self.advance();
                    let t = self.advance();
                    let var = if let Token::Ident(n) = t {
                        n
                    } else {
                        panic!("{t:?} is not a valid enum variant");
                    };
                    Some(Ast::ExprEnumVar { name, var })
                } else {
                    Some(Ast::ExprVar(name))
                }
            }
            TokenKind::LParen => {
                self.eat(TokenKind::LParen, "Expected '('");

                // Re-enable struct parsing inside parentheses
                let old_can_struct = self.can_struct;
                self.can_struct = true;

                let expr = self.expression();

                // Restore previous state (e.g., if we were in an 'if' condition)
                self.can_struct = old_can_struct;

                self.eat(TokenKind::RParen, "Expected ')' after expression");
                expr
            }
            TokenKind::LSquare => {
                // F5: `[...]` produces a list literal. The
                // earlier `ExprArrayInit` AST node is kept as
                // dead code for the OpenPrem typed-array port
                // but is never emitted by the parser anymore —
                // no flows use it and the heterogeneous list
                // type covers the cases that matter for
                // operator-authored flows.
                self.advance();
                let mut elements = Vec::new();
                while !matches!(self.current(), Token::RSquare) {
                    elements.push(self.expression()?);
                    if self.tokens[self.index].get_kind() == TokenKind::Comma {
                        self.index += 1;
                    } else {
                        break;
                    }
                }
                self.eat(TokenKind::RSquare, "expected `]` to close a list literal");
                Some(Ast::ExprList { elements })
            }
            // F7: `{ "k": v, ... }` map literal. Only fires
            // when `can_struct` is true — same gate that
            // distinguishes `Ident { foo: 1 }` struct init from
            // an `if cond { body }` body brace. Inside an
            // if/while/for condition the parser disables
            // `can_struct` (see `if_stmt` etc.), so `LCurly`
            // there will fall through to the unrecognised-token
            // branch — preserving the pre-existing behaviour
            // for those positions.
            TokenKind::LCurly if self.can_struct => {
                self.advance();
                let mut pairs: Vec<(String, Ast)> = Vec::new();
                while !matches!(self.current(), Token::RCurly) {
                    let key = match self.advance() {
                        Token::String(s) => s,
                        other => {
                            self.debtok(4);
                            panic!("map literal keys must be string literals, got {other:?}");
                        }
                    };
                    self.eat(TokenKind::Colon, "expected `:` between map key and value");
                    let value = self.expression()?;
                    pairs.push((key, value));
                    if self.tokens[self.index].get_kind() == TokenKind::Comma {
                        self.index += 1;
                    } else {
                        break;
                    }
                }
                self.eat(TokenKind::RCurly, "expected `}` to close a map literal");
                Some(Ast::ExprMap { pairs })
            }
            x => {
                eprintln!("not an expressionable token: {x:?}");
                self.debtok(8);
                None
            }
        };
        if res.is_none() {
            panic!("could not parse expression!");
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_string(ast: &Ast, expected: &str) {
        match ast {
            Ast::ExprString(s) => assert_eq!(s, expected),
            other => panic!("expected ExprString({expected:?}), got {other:?}"),
        }
    }

    fn assert_var(ast: &Ast, expected: &str) {
        match ast {
            Ast::ExprVar(s) => assert_eq!(s, expected),
            other => panic!("expected ExprVar({expected:?}), got {other:?}"),
        }
    }

    #[test]
    fn interp_string_without_markers_returns_plain_expr_string() {
        match expand_string_interpolation("hello world") {
            Ast::ExprString(s) => assert_eq!(s, "hello world"),
            other => panic!("expected ExprString, got {other:?}"),
        }
    }

    #[test]
    fn interp_string_empty_returns_empty_string() {
        match expand_string_interpolation("") {
            Ast::ExprString(s) => assert_eq!(s, ""),
            other => panic!("expected ExprString(\"\"), got {other:?}"),
        }
    }

    #[test]
    fn interp_single_marker_at_start_expands_to_var_plus_suffix() {
        let ast = expand_string_interpolation("{{name}} world");
        match ast {
            Ast::ExprBinary { lhs, rhs, op } => {
                assert!(matches!(op, Token::Plus));
                assert_var(&lhs, "name");
                assert_string(&rhs, " world");
            }
            other => panic!("expected ExprBinary, got {other:?}"),
        }
    }

    #[test]
    fn interp_single_marker_at_end_expands_to_prefix_plus_var() {
        let ast = expand_string_interpolation("hi {{name}}");
        match ast {
            Ast::ExprBinary { lhs, rhs, op } => {
                assert!(matches!(op, Token::Plus));
                assert_string(&lhs, "hi ");
                assert_var(&rhs, "name");
            }
            other => panic!("expected ExprBinary, got {other:?}"),
        }
    }

    #[test]
    fn interp_marker_in_middle_expands_to_three_chunks() {
        // `prefix + name + suffix` folds left:
        //   ((prefix + name) + suffix)
        let ast = expand_string_interpolation("hello {{name}} world");
        match ast {
            Ast::ExprBinary { lhs, rhs, op } => {
                assert!(matches!(op, Token::Plus));
                assert_string(&rhs, " world");
                match *lhs {
                    Ast::ExprBinary {
                        lhs: l2,
                        rhs: r2,
                        op: op2,
                    } => {
                        assert!(matches!(op2, Token::Plus));
                        assert_string(&l2, "hello ");
                        assert_var(&r2, "name");
                    }
                    other => panic!("expected nested ExprBinary, got {other:?}"),
                }
            }
            other => panic!("expected ExprBinary, got {other:?}"),
        }
    }

    #[test]
    fn interp_multiple_markers_chain_left_fold() {
        let ast = expand_string_interpolation("{{a}}{{b}}");
        // Two adjacent markers fold as `a + b`.
        match ast {
            Ast::ExprBinary { lhs, rhs, op } => {
                assert!(matches!(op, Token::Plus));
                assert_var(&lhs, "a");
                assert_var(&rhs, "b");
            }
            other => panic!("expected ExprBinary, got {other:?}"),
        }
    }

    #[test]
    fn interp_whitespace_inside_braces_is_trimmed() {
        let ast = expand_string_interpolation("{{  name  }}");
        assert_var(&ast, "name");
    }

    #[test]
    fn interp_unterminated_open_brace_preserved_verbatim() {
        // Operator typo: missing closing `}}`. We keep the
        // text so they see what went wrong.
        match expand_string_interpolation("hi {{ no closer") {
            Ast::ExprString(s) => assert_eq!(s, "hi {{ no closer"),
            other => panic!("expected ExprString, got {other:?}"),
        }
    }

    #[test]
    fn interp_empty_marker_preserved_verbatim() {
        match expand_string_interpolation("hello {{}} world") {
            Ast::ExprString(s) => assert_eq!(s, "hello {{}} world"),
            other => panic!("expected ExprString, got {other:?}"),
        }
    }

    #[test]
    fn interp_non_identifier_inside_braces_preserved_verbatim() {
        // `{{1+2}}` is not a valid identifier — keep the
        // literal text so the operator sees the typo. SOL
        // doesn't try to be clever about expression
        // interpolation.
        match expand_string_interpolation("v={{1+2}}") {
            Ast::ExprString(s) => assert_eq!(s, "v={{1+2}}"),
            other => panic!("expected ExprString, got {other:?}"),
        }
    }

    #[test]
    fn interp_identifier_with_underscores_and_digits_works() {
        let ast = expand_string_interpolation("{{user_id_42}}");
        assert_var(&ast, "user_id_42");
    }
}
