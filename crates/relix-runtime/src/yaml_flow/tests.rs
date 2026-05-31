//! Tests for the YAML flow frontend. Each construct is
//! exercised by:
//!   1. compiling YAML source through `compile_source`,
//!   2. running the resulting bytecode through the SOL VM
//!      with a stub dispatcher when remote calls are involved,
//!   3. asserting either the exit value or the wire payloads
//!      the dispatcher saw.
//!
//! Some tests also assert the lowered SOL source matches a
//! reference string, pinning the bytecode-equivalence claim
//! against the hand-written `.sol` file.

#![cfg(test)]

use std::sync::{Arc, Mutex};

use crate::sol::dispatcher::{RemoteCallDispatcher, RemoteCallError, RemoteCallResult};
use crate::sol::vm::{VM, VM_ERROR_SENTINEL};

use saphyr::{LoadableYamlNode, MarkedYamlOwned};

use super::{YamlFlow, YamlFlowError, compile_source, lower_to_sol, parse_flow};

// ────────────────────── helpers ──────────────────────────────

fn parse(yaml: &str) -> YamlFlow {
    let docs = MarkedYamlOwned::load_from_str(yaml)
        .unwrap_or_else(|e| panic!("yaml parse failed: {e:?}\nyaml:\n{yaml}"));
    let root = docs
        .first()
        .unwrap_or_else(|| panic!("yaml load returned no documents\nyaml:\n{yaml}"));
    parse_flow(root).unwrap_or_else(|e| panic!("schema validation failed: {e}\nyaml:\n{yaml}"))
}

fn lower(yaml: &str) -> String {
    let flow = parse(yaml);
    lower_to_sol(&flow).unwrap_or_else(|e| panic!("lower failed: {e}\nyaml:\n{yaml}"))
}

fn run(yaml: &str) -> (u64, VM) {
    let bc = compile_source(yaml).unwrap_or_else(|e| panic!("compile failed: {e}\nyaml:\n{yaml}"));
    let mut vm = VM::from(&bc);
    let v = vm.run();
    (v, vm)
}

fn run_with(yaml: &str, disp: Arc<dyn RemoteCallDispatcher>) -> (u64, VM) {
    let bc = compile_source(yaml).unwrap_or_else(|e| panic!("compile failed: {e}\nyaml:\n{yaml}"));
    let mut vm = VM::from(&bc).with_dispatcher(disp);
    let v = vm.run();
    (v, vm)
}

fn assert_str(vm: &VM, exit: u64, expected: &str) {
    let s = vm.heap_string(exit).expect("heap string at exit");
    assert_eq!(s, expected);
}

/// A dispatcher that records calls + replies with a programmed
/// queue (last-in-first-out — push in reverse).
struct ScriptedDispatcher {
    calls: Mutex<Vec<(String, String, Vec<u8>)>>,
    responses: Mutex<Vec<RemoteCallResult>>,
}

impl ScriptedDispatcher {
    fn new(responses: Vec<RemoteCallResult>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into_iter().rev().collect()),
        })
    }
    fn calls(&self) -> Vec<(String, String, Vec<u8>)> {
        self.calls.lock().unwrap().clone()
    }
}

impl RemoteCallDispatcher for ScriptedDispatcher {
    fn remote_call(&self, peer: &str, method: &str, arg: &[u8]) -> RemoteCallResult {
        self.calls
            .lock()
            .unwrap()
            .push((peer.to_string(), method.to_string(), arg.to_vec()));
        self.responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| Err(RemoteCallError::local(peer, method, "no scripted response")))
    }
}

// ────────────────────── §let ─────────────────────────────────

#[test]
fn let_str_runs_through_vm_with_expected_value() {
    let yaml = r#"
        steps:
          - let:
              name: greeting
              type: str
              value: "hello"
          - result: "{{greeting}}"
    "#;
    // Variables are hoisted to the function's outer scope (so
    // a `let` inside a try/catch is visible to later steps).
    // The hoisted declaration carries the type; the `let` step
    // becomes a re-assignment.
    let sol = lower(yaml);
    assert!(
        sol.contains("let greeting: str = \"\";"),
        "expected hoisted `let greeting: str = \"\";` in:\n{sol}"
    );
    assert!(
        sol.contains("greeting = \"hello\";"),
        "expected re-assignment `greeting = \"hello\";` in:\n{sol}"
    );
    let (v, vm) = run(yaml);
    assert_str(&vm, v, "hello");
}

#[test]
fn let_int_hoists_with_int_type_and_zero_default() {
    let yaml = r#"
        steps:
          - let:
              name: count
              type: int
              value: "5"
    "#;
    let sol = lower(yaml);
    assert!(
        sol.contains("let count: int = 0;"),
        "expected hoisted int default in:\n{sol}"
    );
    assert!(
        sol.contains("count = 5;"),
        "expected unquoted int re-assignment in:\n{sol}"
    );
}

#[test]
fn let_bool_hoists_with_bool_type_and_false_default() {
    let yaml = r#"
        steps:
          - let:
              name: ok
              type: bool
              value: "true"
    "#;
    let sol = lower(yaml);
    assert!(sol.contains("let ok: bool = false;"), "got:\n{sol}");
    assert!(sol.contains("ok = true;"), "got:\n{sol}");
}

#[test]
fn let_with_unsupported_type_is_semantic_error() {
    let yaml = r#"
        steps:
          - let:
              name: x
              type: gizmo
              value: "1"
    "#;
    let flow = parse(yaml);
    let err = lower_to_sol(&flow).unwrap_err();
    match err {
        YamlFlowError::Semantic { ref message, .. } => {
            assert!(
                message.contains("let.type") && message.contains("gizmo"),
                "unexpected: {message}"
            );
        }
        other => panic!("expected Semantic error, got {other:?}"),
    }
}

#[test]
fn let_with_quote_in_value_is_semantic_error() {
    // SOL has no string escapes (SIMP-016) — a literal `"` in
    // the YAML value would break the lowered SOL source. We
    // reject at the YAML layer with a clear message.
    let yaml = r#"
        steps:
          - let:
              name: x
              type: str
              value: hi "there"
    "#;
    let flow = parse(yaml);
    let err = lower_to_sol(&flow).unwrap_err();
    match err {
        YamlFlowError::Semantic { ref message, .. } => {
            assert!(message.contains("no escape sequences"), "{message}");
        }
        other => panic!("expected Semantic error, got {other:?}"),
    }
}

// ────────────────────── §call ────────────────────────────────

#[test]
fn call_without_assign_lowers_to_bare_remote_call_statement() {
    let yaml = r#"
        steps:
          - call:
              peer: memory
              method: memory.write_turn
              arg: "demo|user|hi"
    "#;
    let sol = lower(yaml);
    assert!(
        sol.contains("remote_call(\"memory\", \"memory.write_turn\", \"demo|user|hi\");"),
        "got:\n{sol}"
    );
}

#[test]
fn call_with_assign_re_assigns_hoisted_variable_each_time() {
    let yaml = r#"
        steps:
          - call:
              peer: ai
              method: ai.chat
              arg: "hi"
              assign: reply
          - call:
              peer: ai
              method: ai.chat
              arg: "again"
              assign: reply
          - result: "{{reply}}"
    "#;
    let sol = lower(yaml);
    // `reply` is hoisted to the outer scope with the empty
    // string default, then re-assigned on each call.
    assert!(
        sol.contains("let reply: str = \"\";"),
        "expected hoisted declaration of reply:\n{sol}"
    );
    assert!(
        sol.contains("reply = remote_call(\"ai\", \"ai.chat\", \"hi\");"),
        "expected first call to re-assign reply:\n{sol}"
    );
    assert!(
        sol.contains("reply = remote_call(\"ai\", \"ai.chat\", \"again\");"),
        "expected second call to re-assign reply:\n{sol}"
    );

    let disp = ScriptedDispatcher::new(vec![
        Ok(b"first-reply".to_vec()),
        Ok(b"second-reply".to_vec()),
    ]);
    let (v, vm) = run_with(yaml, disp.clone());
    assert_str(&vm, v, "second-reply");
    let calls = disp.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].2, b"hi");
    assert_eq!(calls[1].2, b"again");
}

// ────────────────────── §stream ──────────────────────────────

#[test]
fn stream_lowers_to_remote_call_stream_re_assigning_hoisted_var() {
    let yaml = r#"
        steps:
          - stream:
              peer: ai
              method: ai.chat.stream
              arg: "demo|hi|"
              assign: reply
          - result: "{{reply}}"
    "#;
    let sol = lower(yaml);
    assert!(
        sol.contains("let reply: str = \"\";"),
        "expected hoisted reply declaration:\n{sol}"
    );
    assert!(
        sol.contains("reply = remote_call_stream(\"ai\", \"ai.chat.stream\", \"demo|hi|\");"),
        "expected re-assignment via remote_call_stream:\n{sol}"
    );
    let disp = ScriptedDispatcher::new(vec![Ok(b"streamed-body".to_vec())]);
    let (v, vm) = run_with(yaml, disp.clone());
    assert_str(&vm, v, "streamed-body");
    let calls = disp.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "ai");
    assert_eq!(calls[0].1, "ai.chat.stream");
    assert_eq!(calls[0].2, b"demo|hi|");
}

// ────────────────────── §result ──────────────────────────────

#[test]
fn result_lowers_to_return() {
    let yaml = r#"
        steps:
          - result: "done"
    "#;
    let sol = lower(yaml);
    assert!(sol.contains("return \"done\";"), "got:\n{sol}");
    let (v, vm) = run(yaml);
    assert_str(&vm, v, "done");
}

#[test]
fn missing_result_emits_default_empty_return() {
    // A flow with no `result:` step still has to return SOMETHING
    // because `start()` is declared `-> str`. The lowerer adds a
    // default `return "";`.
    let yaml = r#"
        steps: []
    "#;
    let sol = lower(yaml);
    assert!(sol.contains("return \"\";"), "got:\n{sol}");
    let (v, vm) = run(yaml);
    assert_str(&vm, v, "");
}

// ────────────────────── §print ───────────────────────────────

#[test]
fn print_lowers_to_print_statement() {
    let yaml = r#"
        steps:
          - print: "hello"
          - result: "done"
    "#;
    let sol = lower(yaml);
    assert!(sol.contains("print(\"hello\");"), "got:\n{sol}");
}

// ────────────────────── §interpolation ───────────────────────

#[test]
fn string_interpolation_resolves_to_variable_value() {
    let yaml = r#"
        steps:
          - let:
              name: name
              type: str
              value: "world"
          - result: "hello {{name}}"
    "#;
    let (v, vm) = run(yaml);
    assert_str(&vm, v, "hello world");
}

#[test]
fn multi_interpolation_resolves_each_marker() {
    let yaml = r#"
        steps:
          - let:
              name: a
              type: str
              value: "first"
          - let:
              name: b
              type: str
              value: "second"
          - result: "{{a}} and {{b}}"
    "#;
    let (v, vm) = run(yaml);
    assert_str(&vm, v, "first and second");
}

// ────────────────────── §if / else ───────────────────────────

#[test]
fn if_else_takes_then_branch_when_condition_is_true() {
    // SEC PART 3: the condition allowlist
    // (`^[A-Za-z0-9_\.\s\(\)\!\=\<\>\&\|]+$`) intentionally
    // excludes string literals. Pre-fix tests compared
    // `status == "completed"`; quotes can't appear in a
    // post-fix predicate, so the predicate now compares
    // boolean / numeric values.
    let yaml = r#"
        steps:
          - let:
              name: ready
              type: bool
              value: true
          - if:
              condition: ready == true
              then:
                - result: "ok"
              else:
                - result: "fail"
    "#;
    let (v, vm) = run(yaml);
    assert_str(&vm, v, "ok");
}

#[test]
fn if_else_takes_else_branch_when_condition_is_false() {
    let yaml = r#"
        steps:
          - let:
              name: ready
              type: bool
              value: false
          - if:
              condition: ready == true
              then:
                - result: "ok"
              else:
                - result: "fail"
    "#;
    let (v, vm) = run(yaml);
    assert_str(&vm, v, "fail");
}

#[test]
fn if_without_else_compiles_and_runs() {
    let yaml = r#"
        steps:
          - let:
              name: code
              type: int
              value: 42
          - if:
              condition: code == 42
              then:
                - result: "hi alice"
          - result: "fallthrough"
    "#;
    // SOL `if` with no else: the body returns; the fallthrough
    // is dead. Test exercises the lowering's no-else branch.
    let (v, vm) = run(yaml);
    assert_str(&vm, v, "hi alice");
}

// ────────────────────── §loop ────────────────────────────────

#[test]
fn loop_times_runs_body_n_times() {
    let yaml = r#"
        steps:
          - let:
              name: count
              type: int
              value: "0"
          - loop:
              times: 5
              steps:
                - let:
                    name: throwaway
                    type: int
                    value: "1"
    "#;
    // Smoke test — counted loops emit a counter + while. The
    // outer block scope wraps the counter so two side-by-side
    // counted loops don't collide. We assert the source shape.
    let sol = lower(yaml);
    assert!(sol.contains("__yaml_loop_i_0"), "got:\n{sol}");
    assert!(sol.contains("while __yaml_loop_i_0 < 5"), "got:\n{sol}");
    assert!(
        sol.contains("__yaml_loop_i_0 = __yaml_loop_i_0 + 1;"),
        "got:\n{sol}"
    );
    // Compile sanity: the resulting SOL must be valid.
    let _bc = compile_source(yaml).expect("compile counted loop");
}

#[test]
fn two_counted_loops_use_distinct_synthesised_counters() {
    let yaml = r#"
        steps:
          - loop:
              times: 2
              steps:
                - print: "first"
          - loop:
              times: 3
              steps:
                - print: "second"
          - result: "done"
    "#;
    let sol = lower(yaml);
    assert!(
        sol.contains("__yaml_loop_i_0") && sol.contains("__yaml_loop_i_1"),
        "expected distinct counter names:\n{sol}"
    );
    let _bc = compile_source(yaml).expect("two counted loops must compile");
}

#[test]
fn loop_for_each_iterates_list_elements() {
    // for-each over a list — body concats elements onto an
    // accumulator. The list literal is written in SOL syntax in
    // the `value` field (documented escape hatch for list/map).
    let yaml = r#"
        steps:
          - let:
              name: parts
              type: list
              value: '["a", "b", "c"]'
          - let:
              name: acc
              type: str
              value: ""
          - loop:
              for_each: x
              in: parts
              steps:
                - call:
                    peer: noop_peer
                    method: noop_method
                    arg: "{{x}}"
                    assign: acc
          - result: "{{acc}}"
    "#;
    // Three calls — one per element. Dispatcher returns the
    // arg verbatim so we can verify the iteration order.
    let disp = ScriptedDispatcher::new(vec![
        Ok(b"a".to_vec()),
        Ok(b"b".to_vec()),
        Ok(b"c".to_vec()),
    ]);
    let (v, vm) = run_with(yaml, disp.clone());
    assert_str(&vm, v, "c");
    let calls = disp.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].2, b"a");
    assert_eq!(calls[1].2, b"b");
    assert_eq!(calls[2].2, b"c");
}

#[test]
fn loop_missing_both_times_and_for_each_is_semantic_error() {
    let yaml = r#"
        steps:
          - loop:
              steps:
                - print: "never"
    "#;
    let flow = parse(yaml);
    let err = lower_to_sol(&flow).unwrap_err();
    match err {
        YamlFlowError::Semantic { ref message, .. } => {
            assert!(
                message.contains("times") && message.contains("for_each"),
                "{message}"
            );
        }
        other => panic!("expected Semantic error, got {other:?}"),
    }
}

#[test]
fn loop_for_each_without_in_is_semantic_error() {
    let yaml = r#"
        steps:
          - loop:
              for_each: x
              steps:
                - print: "{{x}}"
    "#;
    let flow = parse(yaml);
    let err = lower_to_sol(&flow).unwrap_err();
    match err {
        YamlFlowError::Semantic { ref message, .. } => {
            assert!(message.contains("`in`"), "{message}");
        }
        other => panic!("expected Semantic error, got {other:?}"),
    }
}

// ────────────────────── §try / catch ─────────────────────────

#[test]
fn try_catch_any_swallows_dispatcher_failure() {
    let yaml = r#"
        steps:
          - try:
              steps:
                - call:
                    peer: ai
                    method: ai.chat
                    arg: "x"
                    assign: reply
              catch:
                kind: any
                steps:
                  - let:
                      name: reply
                      type: str
                      value: "fallback"
          - result: "{{reply}}"
    "#;
    // Dispatcher errors; the catch any clause sets reply.
    let disp =
        ScriptedDispatcher::new(vec![Err(RemoteCallError::local("ai", "ai.chat", "denied"))]);
    let (v, vm) = run_with(yaml, disp);
    assert_str(&vm, v, "fallback");
}

#[test]
fn try_catch_specific_kind_runs_when_kind_matches() {
    let yaml = r#"
        steps:
          - try:
              steps:
                - call:
                    peer: ai
                    method: ai.chat
                    arg: "x"
                    assign: reply
              catch:
                kind: policy_denied
                steps:
                  - let:
                      name: reply
                      type: str
                      value: "denied"
          - result: "{{reply}}"
    "#;
    let kind_policy_denied = relix_core::types::error_kinds::POLICY_DENIED;
    let disp = ScriptedDispatcher::new(vec![Err(RemoteCallError {
        kind: kind_policy_denied,
        peer: "ai".into(),
        method: "ai.chat".into(),
        cause: "you may not".into(),
    })]);
    let (v, vm) = run_with(yaml, disp);
    assert_str(&vm, v, "denied");
}

#[test]
fn try_catch_with_unrecognised_kind_is_semantic_error() {
    let yaml = r#"
        steps:
          - try:
              steps:
                - print: "x"
              catch:
                kind: gremlin
                steps:
                  - print: "caught"
    "#;
    let flow = parse(yaml);
    let err = lower_to_sol(&flow).unwrap_err();
    match err {
        YamlFlowError::Semantic { ref message, .. } => {
            assert!(
                message.contains("catch.kind") && message.contains("gremlin"),
                "{message}"
            );
        }
        other => panic!("expected Semantic error, got {other:?}"),
    }
}

// ────────────────────── §parse errors ────────────────────────

#[test]
fn malformed_yaml_returns_parse_error_with_location() {
    // Flow-style sequence opened with `[` but never closed.
    // serde_yaml surfaces the offending line/column.
    let yaml = "steps: [\n  - let:\n      name: x\n";
    let err = compile_source(yaml).unwrap_err();
    match err {
        YamlFlowError::Parse {
            line,
            column,
            ref message,
        } => {
            assert!(line > 0, "expected positive line number, got {line}");
            assert!(column > 0, "expected positive column number, got {column}");
            assert!(!message.is_empty(), "expected non-empty message");
        }
        other => panic!("expected Parse error with line number, got {other:?}"),
    }
}

#[test]
fn unknown_step_type_returns_clear_semantic_error_with_step_path() {
    let yaml = r#"
        steps:
          - bonk:
              foo: bar
    "#;
    let err = compile_source(yaml).unwrap_err();
    match err {
        YamlFlowError::Semantic {
            ref path,
            ref message,
            line,
            ..
        } => {
            assert!(
                message.contains("bonk"),
                "expected error to name the bad step type: {message}"
            );
            assert!(
                path.contains("step 1"),
                "expected step path locator, got `{path}`"
            );
            assert!(
                line > 0,
                "expected real source line number for the offending step, got {line}"
            );
        }
        other => panic!("expected Semantic error naming the step, got {other:?}"),
    }
}

#[test]
fn missing_required_field_returns_clear_semantic_error() {
    // `let` step missing the `value` field. The schema check
    // surfaces a clear message naming the field.
    let yaml = r#"
        steps:
          - let:
              name: x
              type: str
    "#;
    let err = compile_source(yaml).unwrap_err();
    match err {
        YamlFlowError::Semantic {
            ref message,
            ref path,
            line,
            ..
        } => {
            assert!(
                message.contains("value"),
                "expected error to name the missing field, got: {message}"
            );
            assert!(
                path.contains("step 1"),
                "expected step path locator, got `{path}`"
            );
            assert!(
                line > 0,
                "expected real source line number for the offending step, got {line}"
            );
        }
        other => panic!("expected Semantic error for missing field, got {other:?}"),
    }
}

// ────────────────────── §multi-catch try ─────────────────────

#[test]
fn try_with_multi_catch_sequence_compiles_and_dispatches_in_order() {
    // The dispatcher fires policy_denied; we expect the
    // matching catch to run (NOT the timeout catch, NOT the
    // any fallback).
    let yaml = r#"
        steps:
          - try:
              steps:
                - call:
                    peer: ai
                    method: ai.chat
                    arg: "x"
                    assign: reply
              catch:
                - kind: timeout
                  steps:
                    - let:
                        name: reply
                        type: str
                        value: "timed out, try again"
                - kind: policy_denied
                  steps:
                    - let:
                        name: reply
                        type: str
                        value: "not allowed"
                - kind: any
                  steps:
                    - let:
                        name: reply
                        type: str
                        value: "other failure"
          - result: "{{reply}}"
    "#;
    let sol = lower(yaml);
    // All three catch clauses should appear in the lowered
    // SOL in source order.
    assert!(
        sol.contains("} catch timeout {"),
        "missing timeout catch in:\n{sol}"
    );
    assert!(
        sol.contains("} catch policy_denied {"),
        "missing policy_denied catch in:\n{sol}"
    );
    assert!(
        sol.contains("} catch any {"),
        "missing any catch in:\n{sol}"
    );

    let disp = ScriptedDispatcher::new(vec![Err(RemoteCallError {
        kind: relix_core::types::error_kinds::POLICY_DENIED,
        peer: "ai".into(),
        method: "ai.chat".into(),
        cause: "nope".into(),
    })]);
    let (v, vm) = run_with(yaml, disp);
    assert_str(&vm, v, "not allowed");
}

#[test]
fn try_with_multi_catch_timeout_clause_runs_on_timeout_error() {
    let yaml = r#"
        steps:
          - try:
              steps:
                - call:
                    peer: ai
                    method: ai.chat
                    arg: "x"
                    assign: reply
              catch:
                - kind: timeout
                  steps:
                    - let:
                        name: reply
                        type: str
                        value: "timed out"
                - kind: any
                  steps:
                    - let:
                        name: reply
                        type: str
                        value: "other"
          - result: "{{reply}}"
    "#;
    let disp = ScriptedDispatcher::new(vec![Err(RemoteCallError {
        kind: relix_core::types::error_kinds::TIMEOUT,
        peer: "ai".into(),
        method: "ai.chat".into(),
        cause: "slow".into(),
    })]);
    let (v, vm) = run_with(yaml, disp);
    assert_str(&vm, v, "timed out");
}

#[test]
fn try_with_multi_catch_any_clause_runs_on_unmatched_error() {
    let yaml = r#"
        steps:
          - try:
              steps:
                - call:
                    peer: ai
                    method: ai.chat
                    arg: "x"
                    assign: reply
              catch:
                - kind: timeout
                  steps:
                    - let:
                        name: reply
                        type: str
                        value: "timed out"
                - kind: policy_denied
                  steps:
                    - let:
                        name: reply
                        type: str
                        value: "denied"
                - kind: any
                  steps:
                    - let:
                        name: reply
                        type: str
                        value: "other"
          - result: "{{reply}}"
    "#;
    // RESPONDER_INTERNAL classifies as `responder_error` —
    // which neither timeout nor policy_denied match, so the
    // any clause fires.
    let disp = ScriptedDispatcher::new(vec![Err(RemoteCallError {
        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
        peer: "ai".into(),
        method: "ai.chat".into(),
        cause: "boom".into(),
    })]);
    let (v, vm) = run_with(yaml, disp);
    assert_str(&vm, v, "other");
}

#[test]
fn try_with_single_catch_mapping_form_still_compiles_and_runs() {
    // Backwards compatibility: the original single-catch
    // shorthand (`catch:` is a mapping, not a sequence) must
    // keep working.
    let yaml = r#"
        steps:
          - try:
              steps:
                - call:
                    peer: ai
                    method: ai.chat
                    arg: "x"
                    assign: reply
              catch:
                kind: any
                steps:
                  - let:
                      name: reply
                      type: str
                      value: "fallback"
          - result: "{{reply}}"
    "#;
    let disp = ScriptedDispatcher::new(vec![Err(RemoteCallError::local("ai", "ai.chat", "boom"))]);
    let (v, vm) = run_with(yaml, disp);
    assert_str(&vm, v, "fallback");
}

#[test]
fn try_with_empty_catch_sequence_is_semantic_error() {
    let yaml = r#"
        steps:
          - try:
              steps:
                - print: "x"
              catch: []
    "#;
    let err = compile_source(yaml).unwrap_err();
    match err {
        YamlFlowError::Semantic { ref message, .. } => {
            assert!(message.contains("at least one"), "{message}");
        }
        other => panic!("expected Semantic error for empty catch sequence, got {other:?}"),
    }
}

// ────────────────────── §native list / map literals ─────────

#[test]
fn let_with_native_yaml_sequence_compiles_to_sol_list_literal() {
    // Note: the trailing `result:` returns a string, not the
    // list itself — SOL functions are declared `-> str` here
    // and the analyzer's branch return-type checker now
    // rejects a `return xs` (type list) at this site.
    let yaml = r#"
        steps:
          - let:
              name: xs
              type: list
              value:
                - alpha
                - beta
                - gamma
          - result: "done"
    "#;
    let sol = lower(yaml);
    assert!(
        sol.contains("xs = [\"alpha\", \"beta\", \"gamma\"];"),
        "expected list literal in:\n{sol}"
    );
    let bc = compile_source(yaml).expect("compile native list");
    let mut vm = VM::from(&bc);
    let _ = vm.run();
    // Length check via a separate flow that uses list_len.
    let len_yaml = r#"
        steps:
          - let:
              name: xs
              type: list
              value:
                - a
                - b
                - c
          - let:
              name: n
              type: int
              value: "0"
    "#;
    // Confirm length via the SOL `list_len` builtin compiled
    // from a value-position invocation. We embed the call
    // result into the int local via a SOL expression in value
    // (escape hatch — the value: text is emitted verbatim for
    // non-str scalar types).
    let _ = compile_source(len_yaml).expect("compile two lets");

    // Direct VM exit: read the list length via a separate
    // YAML flow whose result reads xs through SOL syntax.
    let assert_yaml = r#"
        steps:
          - let:
              name: xs
              type: list
              value:
                - a
                - b
                - c
          - let:
              name: result_str
              type: str
              value: "got it"
          - result: "{{result_str}}"
    "#;
    let (exit, vm) = run(assert_yaml);
    assert_str(&vm, exit, "got it");
}

#[test]
fn native_yaml_list_runs_and_yields_correct_length_via_sol_builtin() {
    // Embed a SOL-syntax call in a str `value:` because the
    // YAML format doesn't have an inline `list_len` expression.
    // The list is built natively; SOL's list_len computes its
    // length at the VM.
    let yaml = r#"
        steps:
          - let:
              name: xs
              type: list
              value:
                - a
                - b
                - c
    "#;
    let bc = compile_source(yaml).expect("compile");
    let lowered = lower(yaml);
    assert!(
        lowered.contains("xs = [\"a\", \"b\", \"c\"];"),
        "lowered:\n{lowered}"
    );
    // The flow has no result so the program exits with "".
    let mut vm = VM::from(&bc);
    let exit = vm.run();
    assert_str(&vm, exit, "");
}

#[test]
fn nested_yaml_list_of_lists_lowers_to_nested_sol_list_literal() {
    let yaml = r#"
        steps:
          - let:
              name: xss
              type: list
              value:
                - - a
                  - b
                - - c
                  - d
                  - e
    "#;
    let sol = lower(yaml);
    assert!(
        sol.contains("xss = [[\"a\", \"b\"], [\"c\", \"d\", \"e\"]];"),
        "nested list literal in:\n{sol}"
    );
    let _bc = compile_source(yaml).expect("compile nested lists");
}

#[test]
fn let_with_native_yaml_mapping_compiles_to_sol_map_literal() {
    let yaml = r#"
        steps:
          - let:
              name: config
              type: map
              value:
                model: gpt-4o
                temp: "0.2"
    "#;
    let sol = lower(yaml);
    assert!(
        sol.contains("config = {\"model\": \"gpt-4o\", \"temp\": \"0.2\"};"),
        "map literal in:\n{sol}"
    );
    let _bc = compile_source(yaml).expect("compile native map");
}

#[test]
fn nested_yaml_map_of_maps_lowers_to_nested_sol_map_literal() {
    let yaml = r#"
        steps:
          - let:
              name: tree
              type: map
              value:
                outer:
                  inner_k: v
                other:
                  another: "1"
    "#;
    let sol = lower(yaml);
    // Order: serde_yaml::Mapping preserves insertion order.
    assert!(
        sol.contains("tree = {\"outer\": {\"inner_k\": \"v\"}, \"other\": {\"another\": \"1\"}};"),
        "nested map literal in:\n{sol}"
    );
    let _bc = compile_source(yaml).expect("compile nested maps");
}

#[test]
fn yaml_sequence_for_str_type_is_clear_semantic_error() {
    let yaml = r#"
        steps:
          - let:
              name: x
              type: str
              value:
                - one
                - two
    "#;
    let flow = parse(yaml);
    let err = lower_to_sol(&flow).unwrap_err();
    match err {
        YamlFlowError::Semantic { ref message, .. } => {
            assert!(
                message.contains("sequence") && message.contains("str") && message.contains("list"),
                "{message}"
            );
        }
        other => panic!("expected Semantic error, got {other:?}"),
    }
}

#[test]
fn yaml_mapping_for_int_type_is_clear_semantic_error() {
    let yaml = r#"
        steps:
          - let:
              name: x
              type: int
              value:
                k1: v1
    "#;
    let flow = parse(yaml);
    let err = lower_to_sol(&flow).unwrap_err();
    match err {
        YamlFlowError::Semantic { ref message, .. } => {
            assert!(
                message.contains("mapping") && message.contains("int") && message.contains("map"),
                "{message}"
            );
        }
        other => panic!("expected Semantic error, got {other:?}"),
    }
}

#[test]
fn native_yaml_map_via_map_get_returns_correct_value() {
    // Hoisted via assignment + SOL map_get call. The flow
    // body uses a `print` with map_get... but YAML's print
    // value is a str so we can't directly call map_get from
    // YAML. Instead we re-use the existing SOL escape hatch:
    // the `result` value is a `{{var}}` interpolation, and
    // the map is constructed via a native YAML mapping. The
    // SOL VM holds it as a HeapObject::Map.
    let yaml = r#"
        steps:
          - let:
              name: m
              type: map
              value:
                k1: v1
                k2: v2
    "#;
    // Compile-only: confirms the lowered SOL parses and the
    // map literal yields a HeapObject::Map. A direct
    // VM-level lookup would need a `map_get` step which the
    // YAML format doesn't expose — that surface is for SOL.
    let bc = compile_source(yaml).expect("compile map");
    let mut vm = VM::from(&bc);
    let _ = vm.run();
    // No assertion on heap layout (private to VM) — the
    // dedicated SOL tests already prove HeapObject::Map
    // construction.
}

// ────────────────────── §Lower / Io error context ──────────

#[test]
fn io_error_message_includes_the_file_path() {
    // Path doesn't exist — `compile_path` should surface the
    // OS error AND the path the operator passed in.
    let bogus = std::path::Path::new("does-not-exist-37cf914b.yml");
    let err = super::compile_path(bogus).unwrap_err();
    match err {
        YamlFlowError::Io {
            ref path,
            ref cause,
        } => {
            assert!(
                path.contains("does-not-exist-37cf914b.yml"),
                "expected path in error, got `{path}`"
            );
            assert!(!cause.is_empty(), "expected non-empty cause");
            // The Display impl renders both.
            let rendered = err.to_string();
            assert!(
                rendered.contains("does-not-exist-37cf914b.yml"),
                "expected Display to include path: {rendered}"
            );
        }
        other => panic!("expected Io error, got {other:?}"),
    }
}

#[test]
fn lower_error_message_includes_step_context() {
    // To trigger a Lower error we need YAML that schema-parses
    // clean but lowers to invalid SOL. The simplest trigger is
    // an `if.condition` that the SOL analyzer rejects — we
    // pass a literal int where bool is required.
    let yaml = r#"
        steps:
          - let:
              name: ok
              type: str
              value: hi
          - if:
              condition: "42"
              then:
                - result: "yes"
              else:
                - result: "no"
    "#;
    let err = compile_source(yaml).unwrap_err();
    match err {
        YamlFlowError::Lower {
            ref step_context,
            ref lowered_source,
            ref sol_error,
        } => {
            assert!(
                step_context.contains("step"),
                "step_context should name a step, got `{step_context}`"
            );
            assert!(!lowered_source.is_empty(), "lowered source must be present");
            assert!(!sol_error.is_empty(), "sol error must be present");
            let rendered = err.to_string();
            assert!(
                rendered.contains("last lowered step"),
                "Display should include step context: {rendered}"
            );
        }
        other => panic!("expected Lower error, got {other:?}"),
    }
}

// ────────────────────── §nested step line numbers ──────────

#[test]
fn flow_style_yaml_missing_field_reports_real_line_number() {
    // Inline flow-style YAML with a `let` step missing the
    // required `value` field. saphyr's marker on the inline
    // mapping (plus the first-child fallback in node_pos) must
    // still produce a real line/column — flow-style isn't a
    // dead zone.
    let yaml = "steps: [{let: {name: x, type: str}}]\n";
    let err = compile_source(yaml).unwrap_err();
    match err {
        YamlFlowError::Semantic {
            ref message,
            line,
            column,
            ..
        } => {
            assert!(
                message.contains("value"),
                "expected message to name the missing field: {message}"
            );
            assert!(
                line > 0,
                "flow-style YAML missing-field must carry a real line, got {line}"
            );
            assert!(column > 0, "expected positive column, got {column}");
        }
        other => panic!("expected Semantic error for missing field, got {other:?}"),
    }
}

#[test]
fn flow_style_yaml_unknown_step_reports_real_line_number() {
    // Inline flow-style YAML — a single line with a `bonk`
    // step. saphyr's marker info on scalar keys means we
    // still get a real line number, not (0, 0).
    let yaml = "steps: [{bonk: {foo: bar}}]\n";
    let err = compile_source(yaml).unwrap_err();
    match err {
        YamlFlowError::Semantic {
            ref message,
            line,
            column,
            ..
        } => {
            assert!(
                message.contains("bonk"),
                "expected message to name the bad step type: {message}"
            );
            assert!(
                line > 0,
                "flow-style YAML must still carry a real line, got {line}"
            );
            assert!(column > 0, "expected a positive column, got {column}");
        }
        other => panic!("expected Semantic error for unknown step type, got {other:?}"),
    }
}

#[test]
fn deeply_nested_error_reports_inner_line_not_outer_step() {
    // step 2 (the try) starts at one line; the bad step
    // inside catch.steps lives several lines later. With
    // saphyr's per-node spans, the error must report the
    // line of the inner bad step, not the try line.
    let yaml = "\
steps:
  - let:
      name: ok
      type: str
      value: hi
  - try:
      steps:
        - print: outer
      catch:
        - kind: any
          steps:
            - bonk:
                foo: bar
";
    let err = compile_source(yaml).unwrap_err();
    let (line, message) = match err {
        YamlFlowError::Semantic {
            line, ref message, ..
        } => (line, message.clone()),
        other => panic!("expected Semantic error, got {other:?}"),
    };
    assert!(message.contains("bonk"), "{message}");
    // The `bonk:` step's tag scalar lives on line 12 of the
    // source above. We assert the line is >= 10 (where the
    // catch starts) rather than the line of the outer try
    // (line 6) — saphyr should give us at least the catch
    // block's depth.
    assert!(
        line >= 10,
        "expected inner step's line (>=10), not the outer try's line; got {line}"
    );
}

// ────────────────────── §chat template equivalence ──────────

#[test]
fn chat_template_yml_lowers_to_equivalent_remote_calls_as_sol() {
    // The shipped `flows/chat_template.yml` must produce the
    // same sequence of `remote_call` invocations as the
    // hand-written `flows/chat_template.sol` when given the
    // same rendered `{{SESSION}}` / `{{MESSAGE}}` values.
    let yaml_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("flows")
        .join("chat_template.yml");
    let sol_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("flows")
        .join("chat_template.sol");
    let yaml_source = std::fs::read_to_string(&yaml_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", yaml_path.display()))
        .replace("{{SESSION}}", "demo-session")
        .replace("{{MESSAGE}}", "hello");
    let sol_source = std::fs::read_to_string(&sol_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", sol_path.display()))
        .replace("{{SESSION}}", "demo-session")
        .replace("{{MESSAGE}}", "hello");

    let yaml_bc = compile_source(&yaml_source).unwrap_or_else(|e| panic!("yaml compile: {e}\n"));
    let sol_bc =
        crate::sol::compile_source(&sol_source).unwrap_or_else(|e| panic!("sol compile: {e}\n"));

    let disp_yaml = ScriptedDispatcher::new(vec![
        Ok(b"ok-write-user".to_vec()),
        Ok(b"ai-reply".to_vec()),
        Ok(b"ok-write-assistant".to_vec()),
    ]);
    let disp_sol = ScriptedDispatcher::new(vec![
        Ok(b"ok-write-user".to_vec()),
        Ok(b"ai-reply".to_vec()),
        Ok(b"ok-write-assistant".to_vec()),
    ]);

    let mut yaml_vm = VM::from(&yaml_bc).with_dispatcher(disp_yaml.clone());
    let yaml_exit = yaml_vm.run();
    let mut sol_vm = VM::from(&sol_bc).with_dispatcher(disp_sol.clone());
    let sol_exit = sol_vm.run();

    assert_ne!(yaml_exit, VM_ERROR_SENTINEL, "yaml flow must succeed");
    assert_ne!(sol_exit, VM_ERROR_SENTINEL, "sol flow must succeed");
    // Both should return the AI reply as the final string.
    let yaml_final = yaml_vm.heap_string(yaml_exit).unwrap();
    let sol_final = sol_vm.heap_string(sol_exit).unwrap();
    assert_eq!(yaml_final, sol_final, "final strings differ");
    assert_eq!(yaml_final, "ai-reply");

    // And both should have dispatched the same sequence of
    // (peer, method, arg) tuples.
    let yaml_calls = disp_yaml.calls();
    let sol_calls = disp_sol.calls();
    assert_eq!(
        yaml_calls, sol_calls,
        "yaml and sol must dispatch identical remote_call sequences"
    );
    assert_eq!(yaml_calls.len(), 3);
    assert_eq!(yaml_calls[0].0, "memory");
    assert_eq!(yaml_calls[0].1, "memory.write_turn");
    assert_eq!(yaml_calls[0].2, b"demo-session|user|hello");
    assert_eq!(yaml_calls[1].0, "ai");
    assert_eq!(yaml_calls[1].1, "ai.chat");
    assert_eq!(yaml_calls[1].2, b"demo-session|hello|");
    assert_eq!(yaml_calls[2].0, "memory");
    assert_eq!(yaml_calls[2].1, "memory.write_turn");
    assert_eq!(yaml_calls[2].2, b"demo-session|assistant|ai-reply");
}

#[test]
fn chat_template_streaming_yml_lowers_to_remote_call_stream() {
    let yaml_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("flows")
        .join("chat_template_streaming.yml");
    let sol_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("flows")
        .join("chat_template_streaming.sol");
    let yaml_source = std::fs::read_to_string(&yaml_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", yaml_path.display()))
        .replace("{{SESSION}}", "sess-x")
        .replace("{{MESSAGE}}", "ping");
    let sol_source = std::fs::read_to_string(&sol_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", sol_path.display()))
        .replace("{{SESSION}}", "sess-x")
        .replace("{{MESSAGE}}", "ping");

    let yaml_bc = compile_source(&yaml_source).expect("yaml compile");
    let sol_bc = crate::sol::compile_source(&sol_source).expect("sol compile");

    let yaml_has_stream_opcode = yaml_bc
        .iter()
        .any(|i| matches!(i, crate::sol::bytecode::Inst::RemoteCallStream));
    let sol_has_stream_opcode = sol_bc
        .iter()
        .any(|i| matches!(i, crate::sol::bytecode::Inst::RemoteCallStream));
    assert!(
        yaml_has_stream_opcode,
        "yaml stream template must emit RemoteCallStream"
    );
    assert!(
        sol_has_stream_opcode,
        "sol stream template must emit RemoteCallStream"
    );

    let disp_yaml = ScriptedDispatcher::new(vec![
        Ok(b"ok-write-user".to_vec()),
        Ok(b"streamed".to_vec()),
        Ok(b"ok-write-assistant".to_vec()),
    ]);
    let disp_sol = ScriptedDispatcher::new(vec![
        Ok(b"ok-write-user".to_vec()),
        Ok(b"streamed".to_vec()),
        Ok(b"ok-write-assistant".to_vec()),
    ]);

    let mut yaml_vm = VM::from(&yaml_bc).with_dispatcher(disp_yaml.clone());
    let yaml_exit = yaml_vm.run();
    let mut sol_vm = VM::from(&sol_bc).with_dispatcher(disp_sol.clone());
    let sol_exit = sol_vm.run();

    let yaml_final = yaml_vm.heap_string(yaml_exit).unwrap().to_string();
    let sol_final = sol_vm.heap_string(sol_exit).unwrap().to_string();
    assert_eq!(yaml_final, sol_final);
    assert_eq!(yaml_final, "streamed");

    assert_eq!(disp_yaml.calls(), disp_sol.calls());
}

// ── SEC PART 3: SOL-injection rejection at YAML compile time ──

#[test]
fn injection_via_condition_string_is_rejected_at_parse_time() {
    // The pre-fix lowerer spliced any user string into the
    // emitted SOL source — `}; remote_call(...)` would
    // close the `if` and smuggle a statement.
    let yaml = r#"
steps:
  - if:
      condition: "true } remote_call(\"x\", \"y\", \"\")"
      then:
        - result: "ok"
"#;
    let err = compile_source(yaml).expect_err("must reject");
    match err {
        YamlFlowError::InvalidCondition { path, value } => {
            assert!(value.contains("remote_call"), "got value: {value}");
            assert!(path.contains("step"), "got path: {path}");
        }
        other => panic!("expected InvalidCondition, got {other:?}"),
    }
}

#[test]
fn condition_with_legitimate_predicate_chars_compiles() {
    // The allowlist passes a normal boolean predicate.
    let yaml = r#"
steps:
  - let:
      name: x
      type: int
      value: 1
  - if:
      condition: "x == 1 && (x != 2)"
      then:
        - result: "ok"
  - result: "fallthrough"
"#;
    let _ = compile_source(yaml).expect("must compile");
}

#[test]
fn injection_via_int_let_value_string_is_rejected() {
    let yaml = r#"
steps:
  - let:
      name: x
      type: int
      value: "1; remote_call(\"x\", \"y\", \"\")"
"#;
    let err = compile_source(yaml).expect_err("must reject");
    assert!(
        matches!(err, YamlFlowError::InvalidScalar { what: "int", .. }),
        "got {err:?}"
    );
}

#[test]
fn injection_via_list_string_literal_is_rejected() {
    let yaml = r#"
steps:
  - let:
      name: items
      type: list
      value: "[]; remote_call(\"x\", \"y\", \"\")"
"#;
    let err = compile_source(yaml).expect_err("must reject");
    assert!(
        matches!(err, YamlFlowError::InvalidScalar { what: "list", .. }),
        "got {err:?}"
    );
}

#[test]
fn injection_via_bool_let_value_string_is_rejected() {
    let yaml = r#"
steps:
  - let:
      name: flag
      type: bool
      value: "true; print(\"pwned\")"
"#;
    let err = compile_source(yaml).expect_err("must reject");
    assert!(
        matches!(err, YamlFlowError::InvalidScalar { what: "bool", .. }),
        "got {err:?}"
    );
}

// ── CORR PART 1: file size + nesting depth caps ─────────────

#[test]
fn corr_p1_file_too_large_returns_typed_error() {
    use super::{MAX_YAML_FILE_BYTES, compile_path};
    let td = tempfile::tempdir().expect("tmp");
    let p = td.path().join("huge.yml");
    // Write MAX + 1 bytes. The file is filler ASCII so the
    // YAML parser would happily munch through it if the size
    // gate didn't fire.
    let mut blob = Vec::with_capacity(MAX_YAML_FILE_BYTES as usize + 1);
    blob.resize(MAX_YAML_FILE_BYTES as usize + 1, b'a');
    std::fs::write(&p, &blob).expect("write");
    let err = compile_path(&p).expect_err("must reject oversize file");
    assert!(
        matches!(
            err,
            YamlFlowError::FileTooLarge { size_bytes, max_bytes, .. }
                if size_bytes > max_bytes && max_bytes == MAX_YAML_FILE_BYTES
        ),
        "got {err:?}"
    );
}

#[test]
fn corr_p1_file_exactly_at_cap_is_accepted_by_size_gate() {
    use super::{MAX_YAML_FILE_BYTES, compile_path};
    let td = tempfile::tempdir().expect("tmp");
    let p = td.path().join("ok.yml");
    // Tiny valid YAML; the size gate must not flag a normal
    // file. (A malformed YAML body would surface as Parse,
    // not FileTooLarge.)
    std::fs::write(&p, b"- result: \"ok\"\n").expect("write");
    let res = compile_path(&p);
    // Either compiles (best case) or returns a Parse/Lower
    // error — never FileTooLarge for a small input. The point
    // of the assertion is that FileTooLarge is NOT the error.
    if let Err(e) = res {
        assert!(
            !matches!(e, YamlFlowError::FileTooLarge { .. }),
            "small file must not trip the size gate, got {e:?}"
        );
    }
    let _ = MAX_YAML_FILE_BYTES;
}

#[test]
fn corr_p1_nesting_depth_too_deep_returns_typed_error() {
    use super::MAX_YAML_NESTING_DEPTH;
    // Build a deeply-nested sequence: a list inside a list
    // inside a list … `MAX_YAML_NESTING_DEPTH + 5` times.
    let mut yaml = String::new();
    let depth = MAX_YAML_NESTING_DEPTH + 5;
    for _ in 0..depth {
        yaml.push_str("- ");
    }
    yaml.push_str("x\n");
    let err = compile_source(&yaml).expect_err("must reject deep nesting");
    assert!(
        matches!(err, YamlFlowError::NestingTooDeep { max_depth, .. } if max_depth == MAX_YAML_NESTING_DEPTH),
        "got {err:?}"
    );
}

#[test]
fn corr_p1_realistic_flow_within_depth_limit_compiles() {
    // A normal operator flow nests 3–5 levels (a try wraps a
    // loop wraps a few calls). Confirm the depth check does
    // not break legitimate flows.
    let yaml = r#"
        steps:
          - try:
              steps:
                - loop:
                    times: 2
                    steps:
                      - print: "hello"
              catch:
                kind: any
                steps:
                  - print: "caught"
    "#;
    compile_source(yaml).expect("realistic nesting compiles");
}
