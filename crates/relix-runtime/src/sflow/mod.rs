//! Sflow — a step-based DSL that compiles in parallel with the Rust-like SOL.
//!
//! Sflow files (`.sflow`) are a flat sequence of statements: capability calls,
//! variable assignments, conditional branches, loops, try/catch blocks, and
//! built-in `sol.*` steps. Unlike SOL there are no function definitions, no
//! type annotations, no `{}` blocks, and no semicolons.
//!
//! The two languages share the same [`RemoteCallDispatcher`] and the same
//! per-flow event log on disk. The executor walks the AST directly — error
//! handling, named-step results, and `${var}` interpolation are simpler to
//! express against a tree than against the SOL VM's stack machine.
//!
//! Entry points:
//! - [`compile`] — parse a `.sflow` source string into a [`Program`].
//! - [`Executor`] — run a compiled [`Program`] against a dispatcher.
//! - [`SflowError`] — parse / runtime error type with line numbers when
//!   available.
//!
//! See `docs/sol.md` for the full language reference and worked examples.

pub mod executor;
pub mod lexer;
pub mod parser;

pub use executor::{ExecOutcome, Executor, RuntimeError};
pub use parser::{Condition, Expr, Program, Stmt, parse};

use std::fmt;

/// Top-level error returned by [`compile`] and the executor when validation
/// fails. The `line` field is 1-indexed when available; `0` means the error
/// is not associated with a specific source line (e.g. budget exhaustion).
#[derive(Clone, Debug)]
pub struct SflowError {
    /// 1-indexed source line, or `0` for non-positional errors.
    pub line: usize,
    /// Human-readable cause.
    pub message: String,
}

impl SflowError {
    pub fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for SflowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}", self.message)
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for SflowError {}

/// Compile a `.sflow` source string. Returns the parsed [`Program`] on success
/// or the first [`SflowError`] encountered (lex or parse).
pub fn compile(source: &str) -> Result<Program, SflowError> {
    let tokens = lexer::tokenize(source)?;
    parser::parse(&tokens)
}

/// Validate a source string. Returns every error found by a single pass of
/// [`compile`] (currently the first parse error — the parser is single-pass
/// and aborts on the first hard failure).
pub fn validate(source: &str) -> Vec<SflowError> {
    match compile(source) {
        Ok(_) => Vec::new(),
        Err(e) => vec![e],
    }
}
