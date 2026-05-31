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

/// P6 — default `[sol] max_steps`. A SOL flow that runs more
/// than this many VM instructions returns
/// [`SolError::FuelExhausted`] without producing a result.
/// Picked conservatively so legitimate flows complete and
/// runaway loops abort quickly.
pub const DEFAULT_MAX_STEPS: u64 = 100_000;

/// P6 — absolute hard ceiling on the fuel an operator can
/// assign to a flow (via either `[sol] max_steps` config or a
/// per-flow `#steps N` directive). Even a config value of
/// `u64::MAX` is clamped to this number — a SOL flow that
/// genuinely needs ten million instructions either has a bug
/// or wants a different runtime.
pub const MAX_STEPS_CEILING: u64 = 10_000_000;

/// P6 — typed errors surfaced by the SOL fuel / parse path.
/// `compile_source` still returns `Result<…, String>` for
/// back-compat with all current callers; new call sites should
/// prefer [`compile_source_with_directives`], which surfaces
/// the typed error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SolError {
    /// VM ran out of fuel (the per-execution max-steps budget
    /// elapsed). Carries the actual number of instructions
    /// executed at the moment the budget hit zero, which is
    /// always equal to the configured budget for that flow
    /// but useful to surface in logs without recomputing.
    #[error("sol VM fuel exhausted after {steps_taken} steps")]
    FuelExhausted { steps_taken: u64 },
    /// `#steps` directive carried a value that doesn't parse
    /// as a `u64`.
    #[error("sol: #steps directive value `{value}` is not a positive integer")]
    BadStepsDirective { value: String },
    /// Compile / parse failure surfaced as a single string —
    /// the existing port's parse pipeline reports its errors
    /// as a Box<dyn Any> panic, so we re-wrap here.
    #[error("sol parse: {0}")]
    Parse(String),
}

/// P6 — compiled-flow bundle: the bytecode plus the fuel
/// budget the VM should run it under. `max_steps` is the
/// effective per-execution fuel after resolving precedence:
///
/// 1. `#steps N` directive at the top of the source (highest
///    priority), CLAMPED to [`MAX_STEPS_CEILING`].
/// 2. operator-supplied default (e.g. from `[sol] max_steps`
///    config), CLAMPED to [`MAX_STEPS_CEILING`].
/// 3. [`DEFAULT_MAX_STEPS`] when the caller passes the
///    sentinel value `0` for the default.
#[derive(Debug, Clone)]
pub struct CompiledFlow {
    pub bytecode: Vec<bytecode::Inst>,
    pub max_steps: u64,
}

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
///
/// P6: any leading `#steps N` directive is silently stripped
/// before lexing. The directive's value is NOT propagated by
/// this function — callers that want to honour the per-flow
/// fuel override use [`compile_source_with_directives`].
pub fn compile_source(source: &str) -> Result<Vec<bytecode::Inst>, String> {
    let (stripped, _maybe_fuel) = strip_steps_directive(source).map_err(|e| e.to_string())?;
    compile_stripped_source(&stripped)
}

/// P6 — typed-error variant of [`compile_source`] that ALSO
/// returns the per-flow fuel budget.
///
/// Resolution order for `CompiledFlow.max_steps`:
///   1. `#steps N` directive at top of source — wins when
///      present.
///   2. `default_max_steps` argument — used when the source
///      has no directive AND the argument is non-zero.
///   3. [`DEFAULT_MAX_STEPS`] — used when the argument is `0`.
///
/// The resolved value is then clamped to [`MAX_STEPS_CEILING`].
pub fn compile_source_with_directives(
    source: &str,
    default_max_steps: u64,
) -> Result<CompiledFlow, SolError> {
    let (stripped, directive_fuel) = strip_steps_directive(source)?;
    let bytecode = compile_stripped_source(&stripped).map_err(SolError::Parse)?;
    let raw = directive_fuel.unwrap_or(if default_max_steps == 0 {
        DEFAULT_MAX_STEPS
    } else {
        default_max_steps
    });
    let max_steps = raw.min(MAX_STEPS_CEILING).max(1);
    Ok(CompiledFlow {
        bytecode,
        max_steps,
    })
}

/// Strip leading `#steps N` (and blank / `#`-comment) lines
/// from `source`. Returns the remainder + the parsed fuel
/// override when present. Multiple `#steps` lines are an
/// error (operators get exactly one directive per flow).
fn strip_steps_directive(source: &str) -> Result<(String, Option<u64>), SolError> {
    let mut directive_fuel: Option<u64> = None;
    let mut consumed_bytes: usize = 0;
    let mut directive_seen = false;
    for raw_line in source.split_inclusive('\n') {
        let trimmed = raw_line.trim_start();
        if trimmed.is_empty() {
            consumed_bytes += raw_line.len();
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("#steps") {
            if directive_seen {
                return Err(SolError::BadStepsDirective {
                    value: "duplicate #steps directive".into(),
                });
            }
            directive_seen = true;
            let value_str = rest.trim();
            // Permit trailing comments after the integer:
            // `#steps 500_000  # tuned for the big report`
            let first_token = value_str
                .split(|c: char| c.is_whitespace() || c == '#')
                .find(|s| !s.is_empty())
                .unwrap_or("");
            // Allow `_` thousands separators for readability.
            let normalised = first_token.replace('_', "");
            let parsed = normalised
                .parse::<u64>()
                .map_err(|_| SolError::BadStepsDirective {
                    value: first_token.to_string(),
                })?;
            if parsed == 0 {
                return Err(SolError::BadStepsDirective {
                    value: first_token.to_string(),
                });
            }
            directive_fuel = Some(parsed);
            consumed_bytes += raw_line.len();
            continue;
        }
        // First non-empty / non-directive line — everything
        // from here on is the SOL source proper.
        break;
    }
    Ok((source[consumed_bytes..].to_string(), directive_fuel))
}

fn compile_stripped_source(source: &str) -> Result<Vec<bytecode::Inst>, String> {
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

/// P6 — typed-error variant of [`compile_path`] that returns
/// the [`CompiledFlow`] (bytecode + resolved fuel budget).
pub fn compile_path_with_directives(
    path: &std::path::Path,
    default_max_steps: u64,
) -> Result<CompiledFlow, SolError> {
    let source = std::fs::read_to_string(path)
        .map_err(|e| SolError::Parse(format!("sol: read {}: {e}", path.display())))?;
    compile_source_with_directives(&source, default_max_steps)
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

    // ─────────────────────────────────────────────────────
    // P6 — `#steps` directive + fuel-resolution tests
    // ─────────────────────────────────────────────────────

    #[test]
    fn p6_compile_with_directives_uses_default_when_no_directive_present() {
        let src = "function start() -> str { return \"ok\"; }\n";
        let compiled = compile_source_with_directives(src, 0).unwrap();
        assert_eq!(compiled.max_steps, DEFAULT_MAX_STEPS);
    }

    #[test]
    fn p6_compile_with_directives_honours_caller_default_when_non_zero() {
        let src = "function start() -> str { return \"ok\"; }\n";
        let compiled = compile_source_with_directives(src, 250).unwrap();
        assert_eq!(compiled.max_steps, 250);
    }

    #[test]
    fn p6_steps_directive_overrides_caller_default() {
        // P6 test: "A flow with #steps 200 overrides the
        // default max_steps for that flow only."
        let src = "#steps 200\nfunction start() -> str { return \"ok\"; }\n";
        let compiled = compile_source_with_directives(src, 100_000).unwrap();
        assert_eq!(compiled.max_steps, 200);
    }

    #[test]
    fn p6_steps_directive_supports_underscore_separators() {
        let src = "#steps 1_500_000\nfunction start() -> str { return \"ok\"; }\n";
        let compiled = compile_source_with_directives(src, 0).unwrap();
        assert_eq!(compiled.max_steps, 1_500_000);
    }

    #[test]
    fn p6_hard_ceiling_cannot_be_exceeded_by_any_config_or_directive() {
        // P6 test: "The hard ceiling of 10_000_000 cannot be
        // exceeded by any config or directive."
        let src = format!(
            "#steps {}\nfunction start() -> str {{ return \"ok\"; }}\n",
            MAX_STEPS_CEILING * 2
        );
        let compiled = compile_source_with_directives(&src, 0).unwrap();
        assert_eq!(compiled.max_steps, MAX_STEPS_CEILING);
        // Caller-supplied default at u64::MAX clamps the same way.
        let plain = "function start() -> str { return \"ok\"; }\n";
        let compiled = compile_source_with_directives(plain, u64::MAX).unwrap();
        assert_eq!(compiled.max_steps, MAX_STEPS_CEILING);
    }

    #[test]
    fn p6_zero_steps_directive_is_rejected() {
        // A zero budget = the VM is instantly out of fuel. The
        // operator almost certainly meant something else;
        // reject at compile time.
        let src = "#steps 0\nfunction start() -> str { return \"ok\"; }\n";
        let res = compile_source_with_directives(src, 0);
        match res {
            Err(SolError::BadStepsDirective { value }) => assert_eq!(value, "0"),
            other => panic!("expected BadStepsDirective, got {other:?}"),
        }
    }

    #[test]
    fn p6_non_integer_steps_directive_is_rejected() {
        let src = "#steps lots\nfunction start() -> str { return \"ok\"; }\n";
        let res = compile_source_with_directives(src, 0);
        match res {
            Err(SolError::BadStepsDirective { value }) => assert_eq!(value, "lots"),
            other => panic!("expected BadStepsDirective, got {other:?}"),
        }
    }

    #[test]
    fn p6_duplicate_steps_directive_is_rejected() {
        let src = "#steps 100\n#steps 200\nfunction start() -> str { return \"ok\"; }\n";
        let res = compile_source_with_directives(src, 0);
        assert!(matches!(res, Err(SolError::BadStepsDirective { .. })));
    }

    #[test]
    fn p6_directive_with_trailing_comment_is_accepted() {
        let src = "#steps 500_000  # tuned for the big report\n\
             function start() -> str { return \"ok\"; }\n";
        let compiled = compile_source_with_directives(src, 0).unwrap();
        assert_eq!(compiled.max_steps, 500_000);
    }

    #[test]
    fn p6_back_compat_compile_source_silently_strips_directive() {
        // compile_source returns Vec<Inst> — operators using
        // the legacy API still get a clean compile when the
        // source has a `#steps` directive (the directive is
        // simply ignored for fuel; the bytecode is identical
        // to a source without the directive).
        let src_with = "#steps 200\nfunction start() -> str { return \"ok\"; }\n";
        let src_without = "function start() -> str { return \"ok\"; }\n";
        let a = compile_source(src_with).unwrap();
        let b = compile_source(src_without).unwrap();
        assert_eq!(a.len(), b.len(), "directive must not change bytecode");
    }
}
