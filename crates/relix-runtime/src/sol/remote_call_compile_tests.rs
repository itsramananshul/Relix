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
fn remote_call_stream_compiles_to_dedicated_opcode() {
    // RELIX-2 step 4: `remote_call_stream("peer", "method", "arg")`
    // must compile to a single Inst::RemoteCallStream opcode
    // with the three string args pushed in source order. The
    // analyzer types the return as `str`, identical to
    // remote_call.
    let src = r#"
        function start() {
            let x: str = remote_call_stream("ai", "ai.chat.stream", "hello");
            print(x);
        }
    "#;
    let bc = compile(src);
    let dis = format!("{bc:?}");
    assert!(
        dis.contains("RemoteCallStream"),
        "expected RemoteCallStream opcode in bytecode, got: {dis}"
    );
    let stream_count = bc
        .iter()
        .filter(|i| matches!(i, Inst::RemoteCallStream))
        .count();
    assert_eq!(
        stream_count, 1,
        "expected exactly one RemoteCallStream opcode"
    );
    let unary_count = bc.iter().filter(|i| matches!(i, Inst::RemoteCall)).count();
    assert_eq!(
        unary_count, 0,
        "remote_call_stream must NOT emit the unary RemoteCall opcode"
    );
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

#[test]
fn try_catch_any_compiles_to_expected_opcode_sequence() {
    let src = r#"
        function start() {
            try {
                let x: str = remote_call("ai", "ai.chat", "hi");
                print(x);
            } catch any {
                print("call failed");
            }
        }
    "#;
    let bc = compile(src);
    let dis = format!("{bc:?}");
    assert!(dis.contains("TryEnter"), "expected TryEnter: {dis}");
    assert!(dis.contains("TryExit"), "expected TryExit: {dis}");
    assert!(dis.contains("RemoteCall"), "expected RemoteCall: {dis}");
    // catch any does NOT emit a LoadErrorKind/EqStr filter —
    // it falls straight through. Confirm by counting EqStr
    // occurrences inside the try block: should be zero.
    let has_kind_filter = bc
        .iter()
        .any(|i| matches!(i, Inst::LoadErrorKind | Inst::EqStr));
    assert!(
        !has_kind_filter,
        "catch any must not emit kind-comparison opcodes"
    );
}

#[test]
fn try_catch_kind_compiles_with_load_error_kind_filter() {
    let src = r#"
        function start() {
            try {
                let x: str = remote_call("ai", "ai.chat", "hi");
                print(x);
            } catch timeout {
                print("timed out");
            } catch any {
                print("other failure");
            }
        }
    "#;
    let bc = compile(src);
    let dis = format!("{bc:?}");
    // Two catch clauses → one LoadErrorKind for the timeout
    // filter and zero LoadErrorKind from the any clause.
    let load_kind_count = bc
        .iter()
        .filter(|i| matches!(i, Inst::LoadErrorKind))
        .count();
    assert_eq!(
        load_kind_count, 1,
        "expected exactly one LoadErrorKind in dispatch table, got bc={dis}"
    );
    // Rethrow lands at the end of the dispatch table as the
    // "no catch matched" fallback.
    assert!(dis.contains("Rethrow"));
}

#[test]
fn delegate_sugar_lowers_to_remote_call_against_delegate_spawn() {
    // `delegate goal G from P to T` must compile to the same
    // shape as `remote_call("coord", "delegate.spawn", <wire>)`.
    // We can't easily assert the exact concat tree (there's no
    // public stringification of the bytecode), but we can
    // assert:
    //
    //   * The string literals "coord" and "delegate.spawn"
    //     appear as PushConst operands.
    //   * Exactly one `RemoteCall` opcode is emitted (the sugar
    //     does not call the capability twice).
    //   * `ConcatStr` opcodes are emitted (the wire payload is
    //     built up at runtime, not inlined as a single literal).
    let src = r#"
        function start() {
            let parent: str = "task-parent";
            let goal: str = "make a sandwich";
            let target: str = "agent-2";
            let child: str = delegate goal goal from parent to target;
            print(child);
        }
    "#;
    let bc = compile(src);
    let dis = format!("{bc:?}");
    assert!(
        dis.contains("\"coord\""),
        "peer literal must be \"coord\": {dis}"
    );
    assert!(
        dis.contains("\"delegate.spawn\""),
        "method literal must be \"delegate.spawn\": {dis}"
    );
    let remote_count = bc.iter().filter(|i| matches!(i, Inst::RemoteCall)).count();
    assert_eq!(
        remote_count, 1,
        "delegate sugar should emit exactly one RemoteCall, got bc={dis}"
    );
    // The wire payload uses the concat machinery, so at least
    // one ConcatStr must appear.
    let concat_count = bc.iter().filter(|i| matches!(i, Inst::ConcatStr)).count();
    assert!(
        concat_count > 0,
        "expected at least one ConcatStr from delegate payload assembly, got bc={dis}"
    );
}

#[test]
fn send_sugar_lowers_to_remote_call_against_msg_send() {
    let src = r#"
        function start() {
            let from_id: str = "agent-1";
            let to_id: str = "agent-2";
            let subj: str = "status";
            let body: str = "all green";
            let msg_id: str = send subject subj body body from from_id to to_id;
            print(msg_id);
        }
    "#;
    let bc = compile(src);
    let dis = format!("{bc:?}");
    assert!(
        dis.contains("\"coord\""),
        "peer literal must be \"coord\": {dis}"
    );
    assert!(
        dis.contains("\"msg.send\""),
        "method literal must be \"msg.send\": {dis}"
    );
    // The tail literal bundles `|||0|sol_flow` into a single
    // PushConst so we look for the substring rather than a
    // standalone "sol_flow" const.
    assert!(
        dis.contains("sol_flow"),
        "origin_surface marker must appear in payload tail: {dis}"
    );
    let remote_count = bc.iter().filter(|i| matches!(i, Inst::RemoteCall)).count();
    assert_eq!(
        remote_count, 1,
        "send sugar should emit exactly one RemoteCall, got bc={dis}"
    );
}

#[test]
fn delegate_without_goal_subkeyword_parses_as_variable() {
    // `delegate` is a soft keyword — without the `goal`
    // follow-up token the parser must fall through to a plain
    // variable reference. The flow uses `delegate` as a normal
    // identifier here and the program must still typecheck.
    let src = r#"
        function start() {
            let delegate: str = "hello";
            print(delegate);
        }
    "#;
    let bc = compile(src);
    let dis = format!("{bc:?}");
    // No RemoteCall opcode should be emitted — this is just a
    // variable load + print, nothing more.
    let remote_count = bc.iter().filter(|i| matches!(i, Inst::RemoteCall)).count();
    assert_eq!(
        remote_count, 0,
        "bare `delegate` identifier must NOT trigger the sugar form: {dis}"
    );
}

#[test]
fn send_without_subject_subkeyword_parses_as_variable() {
    let src = r#"
        function start() {
            let send: str = "world";
            print(send);
        }
    "#;
    let bc = compile(src);
    let remote_count = bc.iter().filter(|i| matches!(i, Inst::RemoteCall)).count();
    assert_eq!(remote_count, 0, "bare `send` identifier must NOT sugar");
}

#[test]
fn delegate_sugar_accepts_string_interpolation_in_goal() {
    // F1 + F3 interop: the goal expression should be able to
    // be a string with `{{var}}` markers. The interpolation
    // happens at parse time (F1), then the resulting concat
    // chain feeds the delegate payload assembly (F3).
    let src = r#"
        function start() {
            let parent: str = "p";
            let name: str = "alice";
            let target: str = "agent-2";
            let child: str = delegate goal "hi {{name}} pls do thing" from parent to target;
            print(child);
        }
    "#;
    let bc = compile(src);
    let dis = format!("{bc:?}");
    // The raw `{{name}}` marker must NOT survive into the
    // bytecode — F1 lowering should have run before F3 saw it.
    assert!(
        !dis.contains("{{name}}"),
        "interpolation marker leaked into delegate sugar payload: {dis}"
    );
    // The prefix literal must still be present.
    assert!(
        dis.contains("\"hi \""),
        "prefix literal from interpolation must survive: {dis}"
    );
    // The bytecode must end up calling delegate.spawn.
    assert!(
        dis.contains("\"delegate.spawn\""),
        "delegate sugar still routes to delegate.spawn: {dis}"
    );
}

#[test]
fn string_interpolation_lowers_to_concat_chain() {
    // `"hi {{name}} bye"` should compile through the same
    // path as `"hi " + name + " bye"`. The bytecode contains
    // a PushVar (or LoadVar-shaped op) referencing `name`
    // and ConcatStr opcodes joining the chunks.
    let src = r#"
        function start() {
            let name: str = "world";
            let s: str = "hi {{name}} bye";
            print(s);
        }
    "#;
    let bc = compile(src);
    let dis = format!("{bc:?}");
    // The literal chunks must appear as separate strings,
    // not as the raw `hi {{name}} bye` blob.
    assert!(
        dis.contains("\"hi \""),
        "expected prefix literal in bytecode: {dis}"
    );
    assert!(
        dis.contains("\" bye\""),
        "expected suffix literal in bytecode: {dis}"
    );
    // The raw marker text must NOT appear in any const slot.
    assert!(
        !dis.contains("{{name}}"),
        "interpolation marker leaked into bytecode unexpanded: {dis}"
    );
}
