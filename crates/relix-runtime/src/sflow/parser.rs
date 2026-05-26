//! Parser for the .sflow DSL.
//!
//! Produces a flat tree of [`Stmt`] from the lexer's token stream. The parser
//! is recursive-descent and threads a `depth` counter through nested block
//! constructs (`if`, `loop`, `while`, `until`, `try`) so deeper-than-8 nesting
//! is rejected at parse time with a line-numbered error.
//!
//! The grammar is line-oriented: most statements consume exactly one source
//! line and the parser uses [`Token::Newline`] as a statement terminator.
//! Block constructs span multiple lines and close on `end`.

use super::SflowError;
use super::lexer::{Lexed, Token};

/// Per-flow nesting cap.
pub const MAX_NESTING_DEPTH: usize = 8;

/// Compiled program. Just a flat list of statements; execution starts at
/// the first one.
#[derive(Clone, Debug)]
pub struct Program {
    pub stmts: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    /// `step <name>: <peer>.<method> <arg>` or bare `<peer>.<method> <arg>`.
    ///
    /// `peer` is the alias used to dial the responder (split off at the
    /// first dot). `wire_method` is the original, unmodified dotted token
    /// — that is the string the responder admits against, so for
    /// `plugin_host.hello.greet` it stays `plugin_host.hello.greet` even
    /// though `peer == "plugin_host"`.
    Step {
        name: Option<String>,
        peer: String,
        wire_method: String,
        arg: Expr,
        line: usize,
    },
    /// `set <name> = <value>`.
    Set {
        name: String,
        value: Expr,
        line: usize,
    },
    /// `if <cond> … [elif <cond> …]* [else …] end`.
    If {
        branches: Vec<(Condition, Vec<Stmt>)>,
        else_body: Option<Vec<Stmt>>,
        line: usize,
    },
    /// `loop <N> times … end`.
    LoopTimes {
        count: u64,
        body: Vec<Stmt>,
        line: usize,
    },
    /// `while <cond> … end`.
    While {
        cond: Condition,
        body: Vec<Stmt>,
        line: usize,
    },
    /// `until <cond> … end`. Same as `while not cond`.
    Until {
        cond: Condition,
        body: Vec<Stmt>,
        line: usize,
    },
    /// F9: `for <ident> in <value_expr> … end`. Iterates the
    /// list resolved from `iter`. Each iteration binds the
    /// element (stringified via `SflowValue::to_display`) to
    /// the loop variable. The variable is scoped to the body
    /// — restored to its previous value (or removed if unset
    /// before the loop) after `end`. Counts toward the
    /// same per-flow `MAX_VARS` cap as `set` does, and the
    /// per-block iteration cap applies the same way it does
    /// to `loop N times` / `while`.
    For {
        var_name: String,
        iter: Expr,
        body: Vec<Stmt>,
        line: usize,
    },
    /// `try … [catch <kind> …]+ end`.
    Try {
        body: Vec<Stmt>,
        catches: Vec<Catch>,
        line: usize,
    },
    /// `rethrow` inside a catch block.
    Rethrow { line: usize },
    /// `return [value]`.
    Return { value: Option<Expr>, line: usize },
    /// `sol.log <message>` — writes a chronicle event without dispatching.
    SolLog { message: Expr, line: usize },
    /// `sol.sleep <seconds>` — pauses execution. Capped at 30s by executor.
    SolSleep { secs: u64, line: usize },
    /// `sol.assert <condition>` — fails the flow if condition is false.
    SolAssert { cond: Condition, line: usize },
    /// `sol.set_result <value>` — sets the flow's running result.
    SolSetResult { value: Expr, line: usize },
}

#[derive(Clone, Debug)]
pub struct Catch {
    pub kind: CatchKind,
    pub body: Vec<Stmt>,
    pub line: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatchKind {
    Timeout,
    MeshError,
    PolicyDenied,
    ResponderError,
    Any,
}

impl CatchKind {
    pub fn from_ident(s: &str) -> Option<Self> {
        Some(match s {
            "timeout" => Self::Timeout,
            "mesh_error" => Self::MeshError,
            "policy_denied" => Self::PolicyDenied,
            "responder_error" => Self::ResponderError,
            "any" => Self::Any,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::MeshError => "mesh_error",
            Self::PolicyDenied => "policy_denied",
            Self::ResponderError => "responder_error",
            Self::Any => "any",
        }
    }
}

/// Right-hand-side / value expression. Strings can carry `${name}` /
/// `${var.x}` / `${loop.iter}` / `${error.kind}` / `${error.message}` /
/// `${step.x.result}` placeholders that the executor expands at run time.
#[derive(Clone, Debug)]
pub enum Expr {
    /// String literal, potentially containing `${…}` interpolations.
    Literal(String),
    /// `result` — the last step's result (raw, no interpolation).
    LastResult,
    /// `var.<name>`.
    Var(String),
    /// `step.<name>.result`.
    StepResult(String),
    /// F5: list literal `[a, b, c]`. Elements are any value
    /// expression; the executor stores the resolved values as
    /// a `SflowValue::List`.
    ListLit(Vec<Expr>),
    /// F7: map literal `{ "k1": v1, "k2": v2 }`. Keys must be
    /// string literals (consistent with SOL); values are any
    /// value expression. The executor stores the resolved
    /// pairs as a `SflowValue::Map`.
    MapLit(Vec<(String, Expr)>),
    /// F6/F8: built-in function call. The parser recognises a
    /// fixed set of names (`list_*`, `map_*`); other call
    /// targets are rejected with a parse error. Arguments are
    /// arbitrary value expressions.
    Call(String, Vec<Expr>),
}

/// Condition used by `if` / `while` / `until` / `sol.assert`.
#[derive(Clone, Debug)]
pub enum Condition {
    True,
    False,
    /// `<atom> <op> <string-literal>` — Eq / Neq / Contains / Matches.
    Compare(Atom, CmpOp, String),
    /// `var.<name> exists` — true iff the variable is set and non-empty.
    Exists(Atom),
    And(Box<Condition>, Box<Condition>),
    Or(Box<Condition>, Box<Condition>),
    Not(Box<Condition>),
}

#[derive(Clone, Debug)]
pub enum Atom {
    /// `status` — last step's status (`completed`, `failed`, `none`).
    Status,
    /// `result` — last step's result.
    Result,
    /// `var.<name>`.
    Var(String),
    /// `step.<name>.status`.
    StepStatus(String),
    /// `step.<name>.result`.
    StepResult(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Neq,
    Contains,
    Matches,
}

/// Parse a token stream into a [`Program`]. Returns the first hard error
/// encountered.
pub fn parse(tokens: &[Lexed]) -> Result<Program, SflowError> {
    let mut p = Parser { tokens, pos: 0 };
    p.skip_newlines();
    let stmts = p.parse_block(0, &[])?;
    p.skip_newlines();
    if p.pos < p.tokens.len() && !matches!(p.peek_token(), Some(Token::Newline) | None) {
        let line = p.peek_line();
        return Err(SflowError::new(
            line,
            format!(
                "unexpected token after end of program: {:?}",
                p.peek_token()
            ),
        ));
    }
    Ok(Program { stmts })
}

struct Parser<'a> {
    tokens: &'a [Lexed],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek_token(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|l| &l.token)
    }

    fn peek_line(&self) -> usize {
        self.tokens
            .get(self.pos)
            .map(|l| l.line)
            .or_else(|| self.tokens.last().map(|l| l.line))
            .unwrap_or(0)
    }

    fn advance(&mut self) -> Option<&'a Lexed> {
        let out = self.tokens.get(self.pos);
        if out.is_some() {
            self.pos += 1;
        }
        out
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek_token(), Some(Token::Newline)) {
            self.pos += 1;
        }
    }

    fn expect_newline(&mut self, ctx: &str) -> Result<(), SflowError> {
        match self.peek_token() {
            Some(Token::Newline) | None => {
                if matches!(self.peek_token(), Some(Token::Newline)) {
                    self.pos += 1;
                }
                Ok(())
            }
            other => Err(SflowError::new(
                self.peek_line(),
                format!("expected end of line after {ctx}, got {other:?}"),
            )),
        }
    }

    /// Parse statements until one of `terminators` is encountered (or EOF).
    /// `depth` is the current nesting level — incremented per nested block
    /// construct (if, loop, while, until, try). Reject anything > 8.
    fn parse_block(
        &mut self,
        depth: usize,
        terminators: &[Token],
    ) -> Result<Vec<Stmt>, SflowError> {
        let mut out = Vec::new();
        loop {
            self.skip_newlines();
            let Some(tok) = self.peek_token() else {
                break;
            };
            if terminators.iter().any(|t| token_kind_eq(t, tok)) {
                break;
            }
            let stmt = self.parse_stmt(depth)?;
            out.push(stmt);
        }
        Ok(out)
    }

    fn parse_stmt(&mut self, depth: usize) -> Result<Stmt, SflowError> {
        let line = self.peek_line();
        let tok = self.peek_token().cloned();
        match tok {
            Some(Token::Step) => self.parse_named_step(line),
            Some(Token::Set) => self.parse_set(line),
            Some(Token::If) => self.parse_if(line, depth),
            Some(Token::Loop) => self.parse_loop_times(line, depth),
            Some(Token::While) => self.parse_while(line, depth),
            Some(Token::Until) => self.parse_until(line, depth),
            Some(Token::For) => self.parse_for(line, depth),
            Some(Token::Try) => self.parse_try(line, depth),
            Some(Token::Rethrow) => {
                self.advance();
                self.expect_newline("rethrow")?;
                Ok(Stmt::Rethrow { line })
            }
            Some(Token::Return) => self.parse_return(line),
            Some(Token::Ident(ref s)) if s == "sol.log" => self.parse_sol_log(line),
            Some(Token::Ident(ref s)) if s == "sol.sleep" => self.parse_sol_sleep(line),
            Some(Token::Ident(ref s)) if s == "sol.assert" => self.parse_sol_assert(line),
            Some(Token::Ident(ref s)) if s == "sol.set_result" => self.parse_sol_set_result(line),
            Some(Token::Ident(_)) => self.parse_unnamed_step(line),
            Some(other) => Err(SflowError::new(
                line,
                format!("expected statement, got {other:?}"),
            )),
            None => Err(SflowError::new(line, "unexpected end of input")),
        }
    }

    fn parse_named_step(&mut self, line: usize) -> Result<Stmt, SflowError> {
        self.advance(); // `step`
        let name = match self.advance() {
            Some(Lexed {
                token: Token::Ident(n),
                ..
            }) => {
                validate_var_name(n, line, "step name")?;
                n.clone()
            }
            other => {
                return Err(SflowError::new(
                    line,
                    format!("expected step name after `step`, got {other:?}"),
                ));
            }
        };
        match self.advance() {
            Some(Lexed {
                token: Token::Colon,
                ..
            }) => {}
            other => {
                return Err(SflowError::new(
                    line,
                    format!("expected `:` after step name, got {other:?}"),
                ));
            }
        }
        let (peer, wire_method, arg) = self.parse_step_call(line)?;
        self.expect_newline("step")?;
        Ok(Stmt::Step {
            name: Some(name),
            peer,
            wire_method,
            arg,
            line,
        })
    }

    fn parse_unnamed_step(&mut self, line: usize) -> Result<Stmt, SflowError> {
        let (peer, wire_method, arg) = self.parse_step_call(line)?;
        self.expect_newline("step")?;
        Ok(Stmt::Step {
            name: None,
            peer,
            wire_method,
            arg,
            line,
        })
    }

    /// Returns `(peer, wire_method, arg)`. `peer` is the alias to dial;
    /// `wire_method` is the entire dotted token the user typed, untouched —
    /// that's what the responder admits against.
    fn parse_step_call(&mut self, line: usize) -> Result<(String, String, Expr), SflowError> {
        let dotted = match self.advance() {
            Some(Lexed {
                token: Token::Ident(s),
                ..
            }) => s.clone(),
            other => {
                return Err(SflowError::new(
                    line,
                    format!("expected `<peer>.<method>`, got {other:?}"),
                ));
            }
        };
        let (peer, _method_suffix) = split_dotted_call(&dotted, line)?;
        let arg = self.parse_step_arg(line)?;
        Ok((peer, dotted, arg))
    }

    /// A step's arg is one of: a quoted string literal, the bareword
    /// `result`, `var.<name>`, `step.<name>.result`, or empty (no arg).
    fn parse_step_arg(&mut self, line: usize) -> Result<Expr, SflowError> {
        match self.peek_token() {
            Some(Token::Newline) | None => Ok(Expr::Literal(String::new())),
            Some(Token::String(_)) => {
                let Some(Lexed {
                    token: Token::String(s),
                    ..
                }) = self.advance()
                else {
                    unreachable!()
                };
                Ok(Expr::Literal(s.clone()))
            }
            Some(Token::Ident(s)) => {
                let val = s.clone();
                self.advance();
                expr_from_ident(&val, line)
            }
            other => Err(SflowError::new(
                line,
                format!("expected step arg, got {other:?}"),
            )),
        }
    }

    fn parse_set(&mut self, line: usize) -> Result<Stmt, SflowError> {
        self.advance(); // `set`
        let name = match self.advance() {
            Some(Lexed {
                token: Token::Ident(n),
                ..
            }) => {
                validate_var_name(n, line, "variable name")?;
                n.clone()
            }
            other => {
                return Err(SflowError::new(
                    line,
                    format!("expected variable name after `set`, got {other:?}"),
                ));
            }
        };
        match self.advance() {
            Some(Lexed {
                token: Token::Eq, ..
            }) => {}
            other => {
                return Err(SflowError::new(
                    line,
                    format!("expected `=` after variable name, got {other:?}"),
                ));
            }
        }
        let value = self.parse_value_expr(line)?;
        self.expect_newline("set")?;
        Ok(Stmt::Set { name, value, line })
    }

    fn parse_value_expr(&mut self, line: usize) -> Result<Expr, SflowError> {
        match self.peek_token() {
            Some(Token::String(_)) => {
                let Some(Lexed {
                    token: Token::String(s),
                    ..
                }) = self.advance()
                else {
                    unreachable!()
                };
                Ok(Expr::Literal(s.clone()))
            }
            // F5: list literal `[a, b, c]` in a value position.
            Some(Token::LSquare) => self.parse_list_literal(line),
            // F7: map literal `{ "k": v, ... }` in a value
            // position. Sflow has no `{` statement so the
            // bracket is unambiguous here.
            Some(Token::LCurly) => self.parse_map_literal(line),
            Some(Token::Ident(s)) => {
                let val = s.clone();
                self.advance();
                // F6/F8: built-in function call. Recognised
                // when the identifier is one of the
                // list_* / map_* names AND the very next
                // token is `(`. Anything else falls back to
                // the existing var / step / result lookup.
                if is_builtin_name(&val) && matches!(self.peek_token(), Some(Token::LParen)) {
                    return self.parse_builtin_call(val, line);
                }
                expr_from_ident(&val, line)
            }
            other => Err(SflowError::new(
                line,
                format!("expected value expression, got {other:?}"),
            )),
        }
    }

    fn parse_list_literal(&mut self, line: usize) -> Result<Expr, SflowError> {
        self.advance(); // consume `[`
        let mut elements: Vec<Expr> = Vec::new();
        loop {
            if matches!(self.peek_token(), Some(Token::RSquare)) {
                break;
            }
            elements.push(self.parse_value_expr(line)?);
            match self.peek_token() {
                Some(Token::Comma) => {
                    self.advance();
                }
                Some(Token::RSquare) => break,
                other => {
                    return Err(SflowError::new(
                        line,
                        format!("expected `,` or `]` in list literal, got {other:?}"),
                    ));
                }
            }
        }
        match self.advance() {
            Some(Lexed {
                token: Token::RSquare,
                ..
            }) => {}
            other => {
                return Err(SflowError::new(
                    line,
                    format!("expected `]` to close list literal, got {other:?}"),
                ));
            }
        }
        Ok(Expr::ListLit(elements))
    }

    fn parse_map_literal(&mut self, line: usize) -> Result<Expr, SflowError> {
        self.advance(); // consume `{`
        let mut pairs: Vec<(String, Expr)> = Vec::new();
        loop {
            if matches!(self.peek_token(), Some(Token::RCurly)) {
                break;
            }
            let key = match self.advance() {
                Some(Lexed {
                    token: Token::String(s),
                    ..
                }) => s.clone(),
                other => {
                    return Err(SflowError::new(
                        line,
                        format!("map literal keys must be string literals, got {other:?}"),
                    ));
                }
            };
            match self.advance() {
                Some(Lexed {
                    token: Token::Colon,
                    ..
                }) => {}
                other => {
                    return Err(SflowError::new(
                        line,
                        format!("expected `:` after map key, got {other:?}"),
                    ));
                }
            }
            let value = self.parse_value_expr(line)?;
            pairs.push((key, value));
            match self.peek_token() {
                Some(Token::Comma) => {
                    self.advance();
                }
                Some(Token::RCurly) => break,
                other => {
                    return Err(SflowError::new(
                        line,
                        format!("expected `,` or `}}` in map literal, got {other:?}"),
                    ));
                }
            }
        }
        match self.advance() {
            Some(Lexed {
                token: Token::RCurly,
                ..
            }) => {}
            other => {
                return Err(SflowError::new(
                    line,
                    format!("expected `}}` to close map literal, got {other:?}"),
                ));
            }
        }
        Ok(Expr::MapLit(pairs))
    }

    fn parse_builtin_call(&mut self, name: String, line: usize) -> Result<Expr, SflowError> {
        // Caller has already consumed the identifier and the
        // very next token is `(`.
        match self.advance() {
            Some(Lexed {
                token: Token::LParen,
                ..
            }) => {}
            other => {
                return Err(SflowError::new(
                    line,
                    format!("expected `(` after {name}, got {other:?}"),
                ));
            }
        }
        let mut args: Vec<Expr> = Vec::new();
        loop {
            if matches!(self.peek_token(), Some(Token::RParen)) {
                break;
            }
            args.push(self.parse_value_expr(line)?);
            match self.peek_token() {
                Some(Token::Comma) => {
                    self.advance();
                }
                Some(Token::RParen) => break,
                other => {
                    return Err(SflowError::new(
                        line,
                        format!("expected `,` or `)` in {name}(...) args, got {other:?}"),
                    ));
                }
            }
        }
        match self.advance() {
            Some(Lexed {
                token: Token::RParen,
                ..
            }) => {}
            other => {
                return Err(SflowError::new(
                    line,
                    format!("expected `)` to close {name}(...), got {other:?}"),
                ));
            }
        }
        Ok(Expr::Call(name, args))
    }

    fn parse_if(&mut self, line: usize, depth: usize) -> Result<Stmt, SflowError> {
        ensure_depth(depth, line)?;
        self.advance(); // `if`
        let cond = self.parse_condition(line)?;
        self.expect_newline("if")?;
        let body = self.parse_block(depth + 1, &[Token::Elif, Token::Else, Token::End])?;
        let mut branches = vec![(cond, body)];
        let mut else_body: Option<Vec<Stmt>> = None;
        loop {
            self.skip_newlines();
            match self.peek_token() {
                Some(Token::Elif) => {
                    let lline = self.peek_line();
                    self.advance();
                    let c = self.parse_condition(lline)?;
                    self.expect_newline("elif")?;
                    let b = self.parse_block(depth + 1, &[Token::Elif, Token::Else, Token::End])?;
                    branches.push((c, b));
                }
                Some(Token::Else) => {
                    self.advance();
                    self.expect_newline("else")?;
                    let b = self.parse_block(depth + 1, &[Token::End])?;
                    else_body = Some(b);
                    break;
                }
                Some(Token::End) => break,
                other => {
                    return Err(SflowError::new(
                        self.peek_line(),
                        format!("expected `elif`, `else`, or `end`, got {other:?}"),
                    ));
                }
            }
        }
        match self.advance() {
            Some(Lexed {
                token: Token::End, ..
            }) => {}
            _ => {
                return Err(SflowError::new(line, "if block not closed with `end`"));
            }
        }
        self.expect_newline("end")?;
        Ok(Stmt::If {
            branches,
            else_body,
            line,
        })
    }

    fn parse_loop_times(&mut self, line: usize, depth: usize) -> Result<Stmt, SflowError> {
        ensure_depth(depth, line)?;
        self.advance(); // `loop`
        let count = match self.advance() {
            Some(Lexed {
                token: Token::Integer(n),
                ..
            }) => {
                if *n < 0 {
                    return Err(SflowError::new(line, "loop count must be non-negative"));
                }
                *n as u64
            }
            other => {
                return Err(SflowError::new(
                    line,
                    format!("expected integer count after `loop`, got {other:?}"),
                ));
            }
        };
        match self.advance() {
            Some(Lexed {
                token: Token::Times,
                ..
            }) => {}
            other => {
                return Err(SflowError::new(
                    line,
                    format!("expected `times` after loop count, got {other:?}"),
                ));
            }
        }
        self.expect_newline("loop header")?;
        let body = self.parse_block(depth + 1, &[Token::End])?;
        self.advance(); // end
        self.expect_newline("end")?;
        Ok(Stmt::LoopTimes { count, body, line })
    }

    fn parse_while(&mut self, line: usize, depth: usize) -> Result<Stmt, SflowError> {
        ensure_depth(depth, line)?;
        self.advance();
        let cond = self.parse_condition(line)?;
        self.expect_newline("while header")?;
        let body = self.parse_block(depth + 1, &[Token::End])?;
        self.advance(); // end
        self.expect_newline("end")?;
        Ok(Stmt::While { cond, body, line })
    }

    fn parse_for(&mut self, line: usize, depth: usize) -> Result<Stmt, SflowError> {
        ensure_depth(depth, line)?;
        self.advance(); // `for`
        let var_name = match self.advance() {
            Some(Lexed {
                token: Token::Ident(n),
                ..
            }) => {
                validate_var_name(n, line, "for-loop variable name")?;
                n.clone()
            }
            other => {
                return Err(SflowError::new(
                    line,
                    format!("expected loop variable name after `for`, got {other:?}"),
                ));
            }
        };
        match self.advance() {
            Some(Lexed {
                token: Token::In, ..
            }) => {}
            other => {
                return Err(SflowError::new(
                    line,
                    format!("expected `in` after `for {var_name}`, got {other:?}"),
                ));
            }
        }
        let iter = self.parse_value_expr(line)?;
        self.expect_newline("for header")?;
        let body = self.parse_block(depth + 1, &[Token::End])?;
        self.advance(); // `end`
        self.expect_newline("end")?;
        Ok(Stmt::For {
            var_name,
            iter,
            body,
            line,
        })
    }

    fn parse_until(&mut self, line: usize, depth: usize) -> Result<Stmt, SflowError> {
        ensure_depth(depth, line)?;
        self.advance();
        let cond = self.parse_condition(line)?;
        self.expect_newline("until header")?;
        let body = self.parse_block(depth + 1, &[Token::End])?;
        self.advance(); // end
        self.expect_newline("end")?;
        Ok(Stmt::Until { cond, body, line })
    }

    fn parse_try(&mut self, line: usize, depth: usize) -> Result<Stmt, SflowError> {
        ensure_depth(depth, line)?;
        self.advance();
        self.expect_newline("try")?;
        let body = self.parse_block(depth + 1, &[Token::Catch, Token::End])?;
        let mut catches: Vec<Catch> = Vec::new();
        loop {
            self.skip_newlines();
            match self.peek_token() {
                Some(Token::Catch) => {
                    let cline = self.peek_line();
                    self.advance();
                    let kind_ident = match self.advance() {
                        Some(Lexed {
                            token: Token::Ident(n),
                            ..
                        }) => n.clone(),
                        other => {
                            return Err(SflowError::new(
                                cline,
                                format!(
                                    "expected error kind after `catch` \
                                     (timeout / mesh_error / policy_denied / \
                                     responder_error / any), got {other:?}"
                                ),
                            ));
                        }
                    };
                    let Some(kind) = CatchKind::from_ident(&kind_ident) else {
                        return Err(SflowError::new(
                            cline,
                            format!("unknown error kind `{kind_ident}`"),
                        ));
                    };
                    self.expect_newline("catch header")?;
                    let cb = self.parse_block(depth + 1, &[Token::Catch, Token::End])?;
                    catches.push(Catch {
                        kind,
                        body: cb,
                        line: cline,
                    });
                }
                Some(Token::End) => break,
                other => {
                    return Err(SflowError::new(
                        self.peek_line(),
                        format!("expected `catch` or `end` after try body, got {other:?}"),
                    ));
                }
            }
        }
        if catches.is_empty() {
            return Err(SflowError::new(
                line,
                "try block must have at least one catch",
            ));
        }
        self.advance(); // end
        self.expect_newline("end")?;
        Ok(Stmt::Try {
            body,
            catches,
            line,
        })
    }

    fn parse_return(&mut self, line: usize) -> Result<Stmt, SflowError> {
        self.advance(); // `return`
        let value = match self.peek_token() {
            Some(Token::Newline) | None => None,
            _ => Some(self.parse_value_expr(line)?),
        };
        self.expect_newline("return")?;
        Ok(Stmt::Return { value, line })
    }

    fn parse_sol_log(&mut self, line: usize) -> Result<Stmt, SflowError> {
        self.advance();
        let message = self.parse_value_expr(line)?;
        self.expect_newline("sol.log")?;
        Ok(Stmt::SolLog { message, line })
    }

    fn parse_sol_sleep(&mut self, line: usize) -> Result<Stmt, SflowError> {
        self.advance();
        let secs = match self.advance() {
            Some(Lexed {
                token: Token::Integer(n),
                ..
            }) => {
                if *n < 0 {
                    return Err(SflowError::new(
                        line,
                        "sol.sleep needs a non-negative integer",
                    ));
                }
                *n as u64
            }
            other => {
                return Err(SflowError::new(
                    line,
                    format!("sol.sleep expects an integer, got {other:?}"),
                ));
            }
        };
        self.expect_newline("sol.sleep")?;
        Ok(Stmt::SolSleep { secs, line })
    }

    fn parse_sol_assert(&mut self, line: usize) -> Result<Stmt, SflowError> {
        self.advance();
        let cond = self.parse_condition(line)?;
        self.expect_newline("sol.assert")?;
        Ok(Stmt::SolAssert { cond, line })
    }

    fn parse_sol_set_result(&mut self, line: usize) -> Result<Stmt, SflowError> {
        self.advance();
        let value = self.parse_value_expr(line)?;
        self.expect_newline("sol.set_result")?;
        Ok(Stmt::SolSetResult { value, line })
    }

    // ---- Conditions ---------------------------------------------------

    fn parse_condition(&mut self, line: usize) -> Result<Condition, SflowError> {
        self.parse_cond_or(line)
    }

    fn parse_cond_or(&mut self, line: usize) -> Result<Condition, SflowError> {
        let mut lhs = self.parse_cond_and(line)?;
        while matches!(self.peek_token(), Some(Token::Or)) {
            self.advance();
            let rhs = self.parse_cond_and(line)?;
            lhs = Condition::Or(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_cond_and(&mut self, line: usize) -> Result<Condition, SflowError> {
        let mut lhs = self.parse_cond_not(line)?;
        while matches!(self.peek_token(), Some(Token::And)) {
            self.advance();
            let rhs = self.parse_cond_not(line)?;
            lhs = Condition::And(Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_cond_not(&mut self, line: usize) -> Result<Condition, SflowError> {
        if matches!(self.peek_token(), Some(Token::Not)) {
            self.advance();
            let inner = self.parse_cond_not(line)?;
            return Ok(Condition::Not(Box::new(inner)));
        }
        self.parse_cond_atom(line)
    }

    fn parse_cond_atom(&mut self, line: usize) -> Result<Condition, SflowError> {
        match self.peek_token().cloned() {
            Some(Token::True) => {
                self.advance();
                Ok(Condition::True)
            }
            Some(Token::False) => {
                self.advance();
                Ok(Condition::False)
            }
            Some(Token::Ident(name)) => {
                self.advance();
                let atom = atom_from_ident(&name, line)?;
                // Disambiguate: `exists` is unary postfix, otherwise expect cmp op + rhs.
                match self.peek_token().cloned() {
                    Some(Token::Exists) => {
                        self.advance();
                        Ok(Condition::Exists(atom))
                    }
                    Some(Token::EqEq) => {
                        self.advance();
                        let rhs = self.expect_str_literal(line, "`==`")?;
                        Ok(Condition::Compare(atom, CmpOp::Eq, rhs))
                    }
                    Some(Token::BangEq) => {
                        self.advance();
                        let rhs = self.expect_str_literal(line, "`!=`")?;
                        Ok(Condition::Compare(atom, CmpOp::Neq, rhs))
                    }
                    Some(Token::Contains) => {
                        self.advance();
                        let rhs = self.expect_str_literal(line, "`contains`")?;
                        Ok(Condition::Compare(atom, CmpOp::Contains, rhs))
                    }
                    Some(Token::Matches) => {
                        self.advance();
                        let rhs = self.expect_str_literal(line, "`matches`")?;
                        Ok(Condition::Compare(atom, CmpOp::Matches, rhs))
                    }
                    other => Err(SflowError::new(
                        line,
                        format!(
                            "expected `==`, `!=`, `contains`, `matches`, or `exists` after `{name}`, got {other:?}"
                        ),
                    )),
                }
            }
            other => Err(SflowError::new(
                line,
                format!("expected condition, got {other:?}"),
            )),
        }
    }

    fn expect_str_literal(&mut self, line: usize, ctx: &str) -> Result<String, SflowError> {
        match self.advance() {
            Some(Lexed {
                token: Token::String(s),
                ..
            }) => Ok(s.clone()),
            other => Err(SflowError::new(
                line,
                format!("expected string literal after {ctx}, got {other:?}"),
            )),
        }
    }
}

fn token_kind_eq(a: &Token, b: &Token) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

/// Recognise a Sflow built-in by name. Used by `parse_value_expr`
/// to disambiguate `ident(` from `ident` followed by an
/// independent expression on the next line.
pub fn is_builtin_name(s: &str) -> bool {
    matches!(
        s,
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
    )
}

fn ensure_depth(depth: usize, line: usize) -> Result<(), SflowError> {
    if depth >= MAX_NESTING_DEPTH {
        return Err(SflowError::new(
            line,
            format!(
                "block nesting depth exceeds {MAX_NESTING_DEPTH} (deeper trees become unreadable; refactor into smaller flows)"
            ),
        ));
    }
    Ok(())
}

/// Variable / step names: alphanumeric + underscore, max 32 chars, must start
/// with a letter or underscore.
pub fn validate_var_name(name: &str, line: usize, what: &str) -> Result<(), SflowError> {
    if name.is_empty() {
        return Err(SflowError::new(line, format!("{what} is empty")));
    }
    if name.len() > 32 {
        return Err(SflowError::new(
            line,
            format!("{what} `{name}` is longer than 32 characters"),
        ));
    }
    let first = name.chars().next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(SflowError::new(
            line,
            format!("{what} `{name}` must start with a letter or underscore"),
        ));
    }
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return Err(SflowError::new(
                line,
                format!("{what} `{name}` contains illegal character `{c}`"),
            ));
        }
    }
    Ok(())
}

fn split_dotted_call(dotted: &str, line: usize) -> Result<(String, String), SflowError> {
    let parts: Vec<&str> = dotted.splitn(2, '.').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Err(SflowError::new(
            line,
            format!("expected `<peer>.<method>`, got `{dotted}`"),
        ));
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

fn expr_from_ident(s: &str, line: usize) -> Result<Expr, SflowError> {
    if s == "result" {
        return Ok(Expr::LastResult);
    }
    if let Some(name) = s.strip_prefix("var.") {
        validate_var_name(name, line, "variable name")?;
        return Ok(Expr::Var(name.to_string()));
    }
    if let Some(rest) = s.strip_prefix("step.") {
        let Some(name) = rest.strip_suffix(".result") else {
            return Err(SflowError::new(
                line,
                format!("expected `step.<name>.result`, got `{s}`"),
            ));
        };
        validate_var_name(name, line, "step name")?;
        return Ok(Expr::StepResult(name.to_string()));
    }
    Err(SflowError::new(
        line,
        format!(
            "unknown bareword `{s}` (expected `result`, `var.<name>`, or `step.<name>.result`)"
        ),
    ))
}

fn atom_from_ident(s: &str, line: usize) -> Result<Atom, SflowError> {
    if s == "status" {
        return Ok(Atom::Status);
    }
    if s == "result" {
        return Ok(Atom::Result);
    }
    if let Some(name) = s.strip_prefix("var.") {
        validate_var_name(name, line, "variable name")?;
        return Ok(Atom::Var(name.to_string()));
    }
    if let Some(rest) = s.strip_prefix("step.") {
        if let Some(name) = rest.strip_suffix(".status") {
            validate_var_name(name, line, "step name")?;
            return Ok(Atom::StepStatus(name.to_string()));
        }
        if let Some(name) = rest.strip_suffix(".result") {
            validate_var_name(name, line, "step name")?;
            return Ok(Atom::StepResult(name.to_string()));
        }
    }
    Err(SflowError::new(
        line,
        format!(
            "unknown condition atom `{s}` \
             (expected `status`, `result`, `var.<name>`, `step.<name>.status`, or `step.<name>.result`)"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sflow::lexer::tokenize;

    fn p(src: &str) -> Result<Program, SflowError> {
        let toks = tokenize(src)?;
        parse(&toks)
    }

    #[test]
    fn unnamed_step_parses() {
        let prog = p("ai.chat \"hi\"\n").unwrap();
        assert_eq!(prog.stmts.len(), 1);
        match &prog.stmts[0] {
            Stmt::Step {
                name,
                peer,
                wire_method,
                ..
            } => {
                assert!(name.is_none());
                assert_eq!(peer, "ai");
                assert_eq!(wire_method, "ai.chat");
            }
            other => panic!("expected step, got {other:?}"),
        }
    }

    /// Three-segment dotted targets (peer + namespaced method, as plugins
    /// produce) must round-trip with `peer` = first segment and
    /// `wire_method` = the entire original string. The first-dot split is
    /// only used to pick the dial alias; nothing else.
    #[test]
    fn three_segment_step_preserves_wire_method() {
        let prog = p("step x: plugin_host.hello.greet \"alice\"\n").unwrap();
        match &prog.stmts[0] {
            Stmt::Step {
                name,
                peer,
                wire_method,
                arg,
                ..
            } => {
                assert_eq!(name.as_deref(), Some("x"));
                assert_eq!(peer, "plugin_host");
                assert_eq!(wire_method, "plugin_host.hello.greet");
                assert!(matches!(arg, Expr::Literal(s) if s == "alice"));
            }
            other => panic!("expected step, got {other:?}"),
        }

        // Unnamed form must behave identically.
        let prog2 = p("plugin_host.plugin.list \"\"\n").unwrap();
        match &prog2.stmts[0] {
            Stmt::Step {
                peer, wire_method, ..
            } => {
                assert_eq!(peer, "plugin_host");
                assert_eq!(wire_method, "plugin_host.plugin.list");
            }
            other => panic!("expected step, got {other:?}"),
        }

        // The classic two-segment form must continue to work; wire_method
        // is the full string the user typed, equal to the original token.
        let prog3 = p("ai.chat \"hi\"\n").unwrap();
        match &prog3.stmts[0] {
            Stmt::Step {
                peer, wire_method, ..
            } => {
                assert_eq!(peer, "ai");
                assert_eq!(wire_method, "ai.chat");
            }
            other => panic!("expected step, got {other:?}"),
        }
    }

    #[test]
    fn named_step_parses() {
        let prog = p("step reply: ai.chat \"hi\"\n").unwrap();
        match &prog.stmts[0] {
            Stmt::Step { name, .. } => assert_eq!(name.as_deref(), Some("reply")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn if_elif_else_end_parses() {
        let src = "if status == \"completed\"\n  return\nelif status == \"failed\"\n  return \"oops\"\nelse\n  return \"idk\"\nend\n";
        let prog = p(src).unwrap();
        match &prog.stmts[0] {
            Stmt::If {
                branches,
                else_body,
                ..
            } => {
                assert_eq!(branches.len(), 2);
                assert!(else_body.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nested_if_four_levels_parses() {
        let src = "if true\nif true\nif true\nif true\nreturn\nend\nend\nend\nend\n";
        p(src).unwrap();
    }

    #[test]
    fn nested_too_deep_rejected_with_line() {
        // 9 nested ifs — 8 is the cap, the 9th must reject.
        let mut src = String::new();
        for _ in 0..9 {
            src.push_str("if true\n");
        }
        src.push_str("return\n");
        for _ in 0..9 {
            src.push_str("end\n");
        }
        let err = p(&src).unwrap_err();
        assert!(err.message.contains("nesting depth"));
        assert_eq!(err.line, 9); // 9th `if` is on line 9
    }

    #[test]
    fn unclosed_if_rejected() {
        let err = p("if true\nreturn\n").unwrap_err();
        assert!(err.message.contains("end") || err.message.contains("`end`"));
    }

    #[test]
    fn loop_n_times_parses() {
        let prog = p("loop 5 times\nsol.log \"x\"\nend\n").unwrap();
        match &prog.stmts[0] {
            Stmt::LoopTimes { count, body, .. } => {
                assert_eq!(*count, 5);
                assert_eq!(body.len(), 1);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn while_parses() {
        let prog = p("while var.x exists\nsol.log \"y\"\nend\n").unwrap();
        assert!(matches!(prog.stmts[0], Stmt::While { .. }));
    }

    #[test]
    fn try_catch_end_parses() {
        let src = "try\nai.chat \"hi\"\ncatch timeout\nsol.set_result \"slow\"\ncatch any\nrethrow\nend\n";
        let prog = p(src).unwrap();
        match &prog.stmts[0] {
            Stmt::Try { body, catches, .. } => {
                assert_eq!(body.len(), 1);
                assert_eq!(catches.len(), 2);
                assert_eq!(catches[0].kind, CatchKind::Timeout);
                assert_eq!(catches[1].kind, CatchKind::Any);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn try_without_catch_rejected() {
        let err = p("try\nai.chat \"x\"\nend\n").unwrap_err();
        assert!(err.message.contains("catch"));
    }

    #[test]
    fn set_variable_parses() {
        let prog = p("set name = \"alice\"\n").unwrap();
        match &prog.stmts[0] {
            Stmt::Set { name, value, .. } => {
                assert_eq!(name, "name");
                assert!(matches!(value, Expr::Literal(s) if s == "alice"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn return_parses() {
        p("return\n").unwrap();
        p("return \"x\"\n").unwrap();
        p("return var.x\n").unwrap();
        p("return step.s.result\n").unwrap();
    }

    #[test]
    fn unknown_keyword_rejected() {
        let err = p("foobar baz\n").unwrap_err();
        assert!(
            err.message.contains("expected `<peer>.<method>`")
                || err.message.contains("expected statement")
                || err.message.contains("unknown")
        );
    }

    #[test]
    fn variable_name_too_long_rejected() {
        let long = "x".repeat(33);
        let err = p(&format!("set {long} = \"y\"\n")).unwrap_err();
        assert!(err.message.contains("longer than 32"));
    }

    #[test]
    fn condition_with_and_or_not_parses() {
        let prog = p("if status == \"completed\" and not var.x exists\nreturn\nend\n").unwrap();
        assert!(matches!(prog.stmts[0], Stmt::If { .. }));
    }

    #[test]
    fn sol_builtins_parse() {
        let prog = p(concat!(
            "sol.log \"hello\"\n",
            "sol.sleep 1\n",
            "sol.assert true\n",
            "sol.set_result \"ok\"\n",
        ))
        .unwrap();
        assert!(matches!(prog.stmts[0], Stmt::SolLog { .. }));
        assert!(matches!(prog.stmts[1], Stmt::SolSleep { secs: 1, .. }));
        assert!(matches!(prog.stmts[2], Stmt::SolAssert { .. }));
        assert!(matches!(prog.stmts[3], Stmt::SolSetResult { .. }));
    }
}
