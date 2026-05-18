//! Compile-pipeline tests for `remote_call` (M6/Step 3).
//!
//! These drive the full lexer → parser → analyzer → codegen pipeline against
//! tiny SOL fragments and assert the bytecode shape. The analyzer exits the
//! process on type errors (an upstream OpenPrem convention — kept for
//! compatibility with existing SOL test fixtures), so negative-arity /
//! negative-type cases are validated by inspection of the analyzer code and
//! by `cargo run` integration scripts rather than by unit tests here.

use std::io::Write;

use crate::sol::bytecode::{Codegen, Inst};
use crate::sol::lexer::Lexer;
use crate::sol::parser::{Ast, Parser};

/// Helper: compile a SOL source fragment to bytecode. The verbatim-port
/// `Lexer::from(path)` reads source from disk, so we materialize a tempfile.
fn compile(source: &str) -> Vec<Inst> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.sol");
    {
        let mut f = std::fs::File::create(&path).expect("create test.sol");
        f.write_all(source.as_bytes()).expect("write source");
    }
    let mut lexer = Lexer::from(path.to_str().expect("utf-8 path"));
    let tokens = lexer.tokens();
    let mut parser = Parser::from(tokens);
    let mut program = parser.run();
    let mut analyzer = crate::sol::analyzer::Analyzer::new();
    analyzer.run(&mut program);
    let mut codegen = Codegen::from(analyzer.tt_arena);
    codegen.gen_bcode(&program)
}

#[test]
fn remote_call_compiles_to_remote_call_opcode() {
    let src = r#"
        function start() {
            let x: str = remote_call("memory", "memory.search", "hello");
            print(x);
        }
    "#;
    let bc = compile(src);
    let dis = format!("{bc:?}");
    assert!(
        dis.contains("RemoteCall"),
        "expected RemoteCall opcode in bytecode, got: {dis}"
    );

    // Verify the three string args are emitted in source order before the opcode.
    let mut peer_idx = None;
    let mut method_idx = None;
    let mut arg_idx = None;
    let mut remote_idx = None;
    for (i, inst) in bc.iter().enumerate() {
        match inst {
            Inst::PushConst(Ast::ExprString(s)) if s == "memory" => peer_idx = Some(i),
            Inst::PushConst(Ast::ExprString(s)) if s == "memory.search" => method_idx = Some(i),
            Inst::PushConst(Ast::ExprString(s)) if s == "hello" => arg_idx = Some(i),
            Inst::RemoteCall => remote_idx = Some(i),
            _ => {}
        }
    }
    let p = peer_idx.expect("peer literal must be emitted");
    let m = method_idx.expect("method literal must be emitted");
    let a = arg_idx.expect("arg literal must be emitted");
    let r = remote_idx.expect("RemoteCall opcode must be emitted");
    assert!(p < m, "peer should be pushed before method");
    assert!(m < a, "method should be pushed before arg");
    assert!(a < r, "all three args should be pushed before RemoteCall");
}

#[test]
fn chained_remote_calls_emit_multiple_opcodes() {
    let src = r#"
        function start() {
            let a: str = remote_call("memory", "node.health", "");
            let b: str = remote_call("ai", "node.health", "");
            print(a);
            print(b);
        }
    "#;
    let bc = compile(src);
    let count = bc
        .iter()
        .filter(|inst| matches!(inst, Inst::RemoteCall))
        .count();
    assert_eq!(count, 2, "expected exactly two RemoteCall opcodes");
}

#[test]
fn codegen_is_deterministic() {
    let src = r#"
        function start() {
            let r: str = remote_call("memory", "node.health", "");
            print(r);
        }
    "#;
    let a = format!("{:?}", compile(src));
    let b = format!("{:?}", compile(src));
    assert_eq!(
        a, b,
        "two compiles of the same source must produce identical bytecode"
    );
}
