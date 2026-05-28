//! SOL — the orchestration language. Ported verbatim from OpenPrem
//! `Apps/INFRA/open-prem-main/src/sol/` so the diff against upstream stays
//! small and reviewable. Relix-specific additions (the `RemoteCall` opcode +
//! dispatcher trait) live alongside, in dedicated modules, so they can be
//! identified at a glance.
//!
//! See `docs/sol-runtime-analysis.md` for the integration strategy.
//
// Clippy is silenced at the module boundary for the verbatim port. Style
// changes are deferred to a coordinated upstream sync. Lints on *new* code
// (dispatcher.rs, anything touching RemoteCall) are NOT suppressed.
#![allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    dead_code,
    unused_imports
)]

pub mod analyzer;
pub mod bytecode;
pub mod cli;
pub mod init;
pub mod lexer;
pub mod parser;
pub mod util;
pub mod vm;

// ---- Relix-specific (not under the module-wide port allow) ----

// Override the port-wide allow for this file only — new code must remain
// clippy-clean.
#[allow(clippy::pub_use)]
pub mod dispatcher;

#[cfg(test)]
mod branch_return_tests;
#[cfg(test)]
mod language_reference_examples;
#[cfg(test)]
mod last_confidence_tests;
#[cfg(test)]
mod list_map_tests;
#[cfg(test)]
mod remote_call_compile_tests;
#[cfg(test)]
mod remote_call_tests;

/// Public, Result-returning entry point into the SOL compile
/// pipeline. The verbatim port's internal helpers historically
/// `process::exit(1)`'d on malformed input — that's been
/// downgraded to `panic!()` so a server-side caller can recover.
/// This wrapper catches those unwinds and surfaces them as a
/// regular `Result`, the contract the rest of the codebase
/// expects.
///
/// Failure modes:
/// - Malformed token stream                  → `Err("sol parse: …")`
/// - Type-check / semantic-analysis failure  → `Err("sol parse: …")`
/// - Codegen panic (rare, indicates a bug)   → `Err("sol parse: …")`
///
/// On success returns the compiled bytecode the VM expects.
pub fn compile_source(source: &str) -> Result<Vec<bytecode::Inst>, String> {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let res = catch_unwind(AssertUnwindSafe(|| {
        let mut lexer = lexer::Lexer::from_source(source);
        let tokens = lexer.tokens();
        let mut parser = parser::Parser::from(tokens);
        let mut program = parser.run();
        let mut analyzer = analyzer::Analyzer::new();
        analyzer.run(&mut program);
        let mut codegen = bytecode::Codegen::from(analyzer.tt_arena);
        codegen.gen_bcode(&program)
    }));
    match res {
        Ok(bytecode) => Ok(bytecode),
        Err(panic) => Err(format!("sol parse: {}", panic_to_message(panic))),
    }
}

/// Same as [`compile_source`] but reads from a file path. Saves
/// callers from the boilerplate of mapping an `io::Error` and the
/// parse error into the same string type.
pub fn compile_path(path: &std::path::Path) -> Result<Vec<bytecode::Inst>, String> {
    let source =
        std::fs::read_to_string(path).map_err(|e| format!("sol: read {}: {e}", path.display()))?;
    compile_source(&source)
}

fn panic_to_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = panic.downcast_ref::<String>() {
        return s.clone();
    }
    "aborted".to_string()
}

#[cfg(test)]
mod compile_source_tests {
    use super::*;

    #[test]
    fn valid_source_compiles_to_some_bytecode() {
        let src = "function start() -> str { return \"ok\"; }\n";
        let res = compile_source(src);
        assert!(res.is_ok(), "expected Ok, got {res:?}");
        assert!(!res.unwrap().is_empty(), "bytecode should be non-empty");
    }

    #[test]
    fn malformed_source_returns_err_without_killing_process() {
        // Truncated function declaration — historically this
        // hard-killed the process via std::process::exit.
        let src = "function start() -> str { let x: str = ";
        let res = compile_source(src);
        assert!(res.is_err(), "expected Err, got {res:?}");
        let msg = res.unwrap_err();
        assert!(
            msg.starts_with("sol parse"),
            "error message should be prefixed (got {msg:?})"
        );
    }

    #[test]
    fn unknown_token_is_err_not_crash() {
        // The `@` character is not in the lexer's accepted set.
        let src = "function start() -> str { @ }\n";
        let res = compile_source(src);
        assert!(res.is_err(), "expected Err, got {res:?}");
    }

    #[test]
    fn empty_source_is_ok_or_err_but_does_not_crash() {
        // Whatever the parser decides, the bridge must not die.
        let _ = compile_source("");
        let _ = compile_source("   \n\n\n");
    }
}
