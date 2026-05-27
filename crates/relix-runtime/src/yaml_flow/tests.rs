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

use super::{YamlFlow, YamlFlowError, compile_source, lower_to_sol, parse_flow};

// ────────────────────── helpers ──────────────────────────────

fn parse(yaml: &str) -> YamlFlow {
    let value: serde_yaml::Value = serde_yaml::from_str(yaml)
        .unwrap_or_else(|e| panic!("yaml parse failed: {e}\nyaml:\n{yaml}"));
    parse_flow(&value).unwrap_or_else(|e| panic!("schema validation failed: {e}\nyaml:\n{yaml}"))
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
    let yaml = r#"
        steps:
          - let:
              name: status
              type: str
              value: "completed"
          - if:
              condition: status == "completed"
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
              name: status
              type: str
              value: "pending"
          - if:
              condition: status == "completed"
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
              name: name
              type: str
              value: "alice"
          - if:
              condition: name == "alice"
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
        } => {
            assert!(
                message.contains("bonk"),
                "expected error to name the bad step type: {message}"
            );
            assert!(
                path.contains("step 1"),
                "expected step path locator, got `{path}`"
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
        } => {
            assert!(
                message.contains("value"),
                "expected error to name the missing field, got: {message}"
            );
            assert!(
                path.contains("step 1"),
                "expected step path locator, got `{path}`"
            );
        }
        other => panic!("expected Semantic error for missing field, got {other:?}"),
    }
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
