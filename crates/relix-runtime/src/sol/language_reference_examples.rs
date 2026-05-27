//! Executable test suite backing every example in
//! `docs/sol-language-reference.md`. Each `#[test]` compiles
//! one example from the reference and asserts the documented
//! result. If a test here fails, either the doc lies or the
//! example was wrong — fix one and rerun.
//!
//! The tests are organized in the same order as the doc
//! sections. Each test's name corresponds to the snippet it
//! exercises.
//!
//! Stub dispatchers are used for examples that involve
//! `remote_call` / `remote_call_stream` so the VM has a real
//! callee to bounce off.

#![cfg(test)]

use std::sync::{Arc, Mutex};

use crate::sol::compile_source;
use crate::sol::dispatcher::{RemoteCallDispatcher, RemoteCallError, RemoteCallResult};
use crate::sol::vm::{VM, VM_ERROR_SENTINEL};

// ─────────────────────── helpers ─────────────────────────────

/// Compile + run, return `(exit_value, vm)`. The exit value is
/// whatever's on top of the operand stack when `run()` returns;
/// for strings that's a heap-string ref, for int/bool that's
/// the raw integer.
fn run(source: &str) -> (u64, VM) {
    let bc = compile_source(source).unwrap_or_else(|e| panic!("compile failed: {e}\n{source}"));
    let mut vm = VM::from(&bc);
    let val = vm.run();
    (val, vm)
}

/// Same as `run` but with a dispatcher attached so flows that
/// call `remote_call` can complete.
fn run_with(source: &str, disp: Arc<dyn RemoteCallDispatcher>) -> (u64, VM) {
    let bc = compile_source(source).unwrap_or_else(|e| panic!("compile failed: {e}\n{source}"));
    let mut vm = VM::from(&bc).with_dispatcher(disp);
    let val = vm.run();
    (val, vm)
}

/// Resolve the exit value as a heap string and compare to
/// `expected`. Panics if the exit value isn't a heap-string
/// ref.
fn assert_str(vm: &VM, exit: u64, expected: &str) {
    let s = vm.heap_string(exit).expect("heap string at exit");
    assert_eq!(s, expected);
}

/// A dispatcher that returns a programmed `Ok` response for
/// every call.
struct OkDispatcher {
    body: Vec<u8>,
    log: Mutex<Vec<(String, String, Vec<u8>)>>,
}

impl OkDispatcher {
    fn new(body: &str) -> Arc<Self> {
        Arc::new(Self {
            body: body.as_bytes().to_vec(),
            log: Mutex::new(Vec::new()),
        })
    }
}

impl RemoteCallDispatcher for OkDispatcher {
    fn remote_call(&self, peer: &str, method: &str, arg: &[u8]) -> RemoteCallResult {
        self.log
            .lock()
            .unwrap()
            .push((peer.to_string(), method.to_string(), arg.to_vec()));
        Ok(self.body.clone())
    }
}

/// A dispatcher that always returns a structured error with
/// the given `kind` and `cause`.
struct ErrDispatcher {
    kind: u32,
    cause: String,
}

impl ErrDispatcher {
    fn new(kind: u32, cause: &str) -> Arc<Self> {
        Arc::new(Self {
            kind,
            cause: cause.to_string(),
        })
    }
}

impl RemoteCallDispatcher for ErrDispatcher {
    fn remote_call(&self, peer: &str, method: &str, _arg: &[u8]) -> RemoteCallResult {
        Err(RemoteCallError {
            kind: self.kind,
            peer: peer.to_string(),
            method: method.to_string(),
            cause: self.cause.clone(),
        })
    }
}

// ─────────────────── §1 Lexical structure ────────────────────

#[test]
fn doc_lexical_line_and_block_comments_compile() {
    let src = r#"
        // line comment is skipped
        /* block comment
           may span lines */
        function start() -> int {
            // also a line comment
            return 42; /* trailing block comment */
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 42);
}

// ──────────────────────── §3.1 Functions ─────────────────────

#[test]
fn doc_forward_function_reference_works() {
    let src = r#"
        function start() -> int {
            return helper();
        }
        function helper() -> int {
            return 7;
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 7);
}

// ─────────────────────── §3.3 Structs ────────────────────────

#[test]
fn doc_struct_field_access_returns_value() {
    let src = r#"
        struct Point {
            x: int,
            y: int,
        }
        function start() -> int {
            let p: Point = Point { x: 3, y: 4 };
            return p.x;
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 3);
}

// ─────────────────── §4.1 Arithmetic ─────────────────────────

#[test]
fn doc_arithmetic_precedence_multiplication_first() {
    let src = r#"function start() -> int { return 2 + 3 * 4; }"#;
    let (v, _) = run(src);
    assert_eq!(v, 14);
}

#[test]
fn doc_string_concat_three_way_is_left_folded() {
    let src = r#"function start() -> str { return "a" + "b" + "c"; }"#;
    let (v, vm) = run(src);
    assert_str(&vm, v, "abc");
}

// ─────────────────── §4.2 Comparison ─────────────────────────

#[test]
fn doc_comparison_greater_than_returns_true() {
    let src = r#"function start() -> bool { return 5 > 3; }"#;
    let (v, _) = run(src);
    assert_eq!(v, 1);
}

// ─────────────────── §4.3 Logical ────────────────────────────

#[test]
fn doc_logical_and_true_false_is_false() {
    let src = r#"function start() -> bool { return true && false; }"#;
    let (v, _) = run(src);
    assert_eq!(v, 0);
}

#[test]
fn doc_logical_or_true_false_is_true() {
    let src = r#"function start() -> bool { return true || false; }"#;
    let (v, _) = run(src);
    assert_eq!(v, 1);
}

#[test]
fn doc_logical_not_false_is_true() {
    let src = r#"function start() -> bool { return !false; }"#;
    let (v, _) = run(src);
    assert_eq!(v, 1);
}

// ─────────────────── §4.4 Bitwise ────────────────────────────

#[test]
fn doc_bitwise_and_12_10_is_8() {
    let src = r#"function start() -> int { return 12 & 10; }"#;
    let (v, _) = run(src);
    assert_eq!(v, 8);
}

// ─────────────────── §4.6 Assignment ─────────────────────────

#[test]
fn doc_assignment_overwrites_local() {
    let src = r#"
        function start() -> int {
            let x: int = 1;
            x = 5;
            return x;
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 5);
}

// ──────────────── §4.7 String interpolation ──────────────────

#[test]
fn doc_interpolation_marker_in_middle_expands() {
    let src = r#"
        function start() -> str {
            let n: str = "world";
            return "hello {{n}}";
        }
    "#;
    let (v, vm) = run(src);
    assert_str(&vm, v, "hello world");
}

#[test]
fn doc_interpolation_empty_marker_preserved_verbatim() {
    let src = r#"
        function start() -> str {
            return "literal {{}}";
        }
    "#;
    let (v, vm) = run(src);
    assert_str(&vm, v, "literal {{}}");
}

// ─────────────── §6.1 if / else / else-if ───────────────────

#[test]
fn doc_else_if_chain_picks_middle_branch() {
    let src = r#"
        function start() -> int {
            let x: int = 2;
            if x == 1 { return 10; }
            else if x == 2 { return 20; }
            else { return 30; }
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 20);
}

// ─────────────────── §6.2 while ──────────────────────────────

#[test]
fn doc_while_loop_sums_first_five_naturals() {
    let src = r#"
        function start() -> int {
            let i: int = 0;
            let sum: int = 0;
            while i < 5 {
                sum = sum + i;
                i = i + 1;
            }
            return sum;
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 10);
}

// ─────────────────── §6.3 for ────────────────────────────────

#[test]
fn doc_for_in_list_concatenates_elements_in_order() {
    let src = r#"
        function start() -> str {
            let xs: list = ["a", "b", "c"];
            let acc: str = "";
            for x in xs {
                acc = acc + x;
            }
            return acc;
        }
    "#;
    let (v, vm) = run(src);
    assert_str(&vm, v, "abc");
}

// ─────────────────── §7.5 List builtins ──────────────────────

#[test]
fn doc_list_len_of_three_element_list_is_three() {
    let src = r#"
        function start() -> int {
            let xs: list = ["a", "b", "c"];
            return list_len(xs);
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 3);
}

#[test]
fn doc_list_get_returns_element_at_index() {
    let src = r#"
        function start() -> str {
            let xs: list = ["alpha", "beta", "gamma"];
            return list_get(xs, 1);
        }
    "#;
    let (v, vm) = run(src);
    assert_str(&vm, v, "beta");
}

#[test]
fn doc_list_get_out_of_bounds_returns_empty_string() {
    let src = r#"
        function start() -> str {
            let xs: list = ["only-one"];
            return list_get(xs, 99);
        }
    "#;
    let (v, vm) = run(src);
    assert_str(&vm, v, "");
}

#[test]
fn doc_list_push_does_not_mutate_original() {
    let src = r#"
        function start() -> int {
            let xs: list = ["a", "b", "c"];
            let ys: list = list_push(xs, "d");
            return list_len(xs);
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 3);
}

#[test]
fn doc_list_push_returned_list_has_pushed_value() {
    let src = r#"
        function start() -> int {
            let xs: list = ["a", "b", "c"];
            let ys: list = list_push(xs, "d");
            return list_len(ys);
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 4);
}

#[test]
fn doc_list_contains_present_value_is_true() {
    let src = r#"
        function start() -> bool {
            let xs: list = ["a", "b", "c"];
            return list_contains(xs, "b");
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 1);
}

#[test]
fn doc_list_contains_absent_value_is_false() {
    let src = r#"
        function start() -> bool {
            let xs: list = ["a", "b", "c"];
            return list_contains(xs, "z");
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 0);
}

#[test]
fn doc_list_join_with_separator_concatenates() {
    let src = r#"
        function start() -> str {
            let xs: list = ["a", "b", "c"];
            return list_join(xs, "-");
        }
    "#;
    let (v, vm) = run(src);
    assert_str(&vm, v, "a-b-c");
}

#[test]
fn doc_list_split_breaks_on_separator() {
    let src = r#"
        function start() -> int {
            let xs: list = list_split("a|b|c", "|");
            return list_len(xs);
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 3);
}

#[test]
fn doc_list_split_empty_input_yields_single_empty_element() {
    let src = r#"
        function start() -> int {
            let xs: list = list_split("", "|");
            return list_len(xs);
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 1);
}

#[test]
fn doc_nested_list_join_uses_inner_pipe_separator() {
    let src = r#"
        function start() -> str {
            let xs: list = [["a", "b"], ["c", "d"]];
            return list_join(xs, ",");
        }
    "#;
    let (v, vm) = run(src);
    assert_str(&vm, v, "a|b,c|d");
}

#[test]
fn doc_list_get_list_on_inner_list_returns_inner_intact() {
    let src = r#"
        function start() -> int {
            let xs: list = [["a", "b", "c"], ["d", "e"]];
            let inner: list = list_get_list(xs, 0);
            return list_len(inner);
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 3);
}

#[test]
fn doc_list_get_list_on_string_element_halts_vm() {
    let src = r#"
        function start() -> int {
            let xs: list = ["scalar"];
            let inner: list = list_get_list(xs, 0);
            return list_len(inner);
        }
    "#;
    let (v, vm) = run(src);
    assert_eq!(v, VM_ERROR_SENTINEL);
    let err = vm
        .last_error()
        .expect("last_error after list_get_list halt");
    assert!(
        err.cause.contains("list_get_list") && err.cause.contains("not a list"),
        "unexpected cause: {}",
        err.cause
    );
}

// ─────────────────── §7.6 Map builtins ───────────────────────

#[test]
fn doc_map_literal_with_two_pairs_lookup_returns_value() {
    let src = r#"
        function start() -> str {
            let m: map = { "k1": "v1", "k2": "v2" };
            return map_get(m, "k1");
        }
    "#;
    let (v, vm) = run(src);
    assert_str(&vm, v, "v1");
}

#[test]
fn doc_map_get_missing_key_returns_empty_string() {
    let src = r#"
        function start() -> str {
            let m: map = { "k1": "v1" };
            return map_get(m, "absent");
        }
    "#;
    let (v, vm) = run(src);
    assert_str(&vm, v, "");
}

#[test]
fn doc_map_has_present_key_is_true() {
    let src = r#"
        function start() -> bool {
            let m: map = { "k1": "v1" };
            return map_has(m, "k1");
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 1);
}

#[test]
fn doc_map_set_does_not_mutate_original() {
    let src = r#"
        function start() -> int {
            let m: map = { "k1": "v1" };
            let m2: map = map_set(m, "k2", "v2");
            return map_len(m);
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 1);
}

#[test]
fn doc_map_set_returned_map_has_new_key() {
    let src = r#"
        function start() -> int {
            let m: map = { "k1": "v1" };
            let m2: map = map_set(m, "k2", "v2");
            return map_len(m2);
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 2);
}

#[test]
fn doc_map_set_overwrites_existing_key() {
    let src = r#"
        function start() -> str {
            let m: map = { "k1": "old" };
            let m2: map = map_set(m, "k1", "new");
            return map_get(m2, "k1");
        }
    "#;
    let (v, vm) = run(src);
    assert_str(&vm, v, "new");
}

#[test]
fn doc_map_keys_preserves_insertion_order() {
    let src = r#"
        function start() -> str {
            let m: map = { "a": "1", "b": "2", "c": "3" };
            let ks: list = map_keys(m);
            return list_get(ks, 0);
        }
    "#;
    let (v, vm) = run(src);
    assert_str(&vm, v, "a");
}

#[test]
fn doc_map_del_removes_named_key() {
    let src = r#"
        function start() -> bool {
            let m: map = { "k1": "v1", "k2": "v2" };
            let m2: map = map_del(m, "k1");
            return map_has(m2, "k1");
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 0);
}

#[test]
fn doc_map_get_map_on_nested_map_returns_inner() {
    let src = r#"
        function start() -> int {
            let m: map = { "outer": { "inner_k": "v" } };
            let inner: map = map_get_map(m, "outer");
            return map_len(inner);
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 1);
}

#[test]
fn doc_map_get_map_missing_key_halts_vm() {
    let src = r#"
        function start() -> int {
            let m: map = { "k1": "v1" };
            let inner: map = map_get_map(m, "absent");
            return map_len(inner);
        }
    "#;
    let (v, vm) = run(src);
    assert_eq!(v, VM_ERROR_SENTINEL);
    let err = vm.last_error().expect("last_error after map_get_map halt");
    assert!(err.cause.contains("not present"), "{}", err.cause);
}

// ─────────────────── §7.2 remote_call ────────────────────────

#[test]
fn doc_remote_call_returns_dispatcher_body_as_string() {
    let src = r#"
        function start() -> str {
            return remote_call("memory", "memory.search", "query");
        }
    "#;
    let (v, vm) = run_with(src, OkDispatcher::new("hi-from-stub"));
    assert_str(&vm, v, "hi-from-stub");
}

#[test]
fn doc_remote_call_failure_with_no_try_halts_vm() {
    let src = r#"
        function start() -> str {
            return remote_call("ai", "ai.chat", "x");
        }
    "#;
    let (v, vm) = run_with(src, ErrDispatcher::new(6, "policy denied"));
    assert_eq!(v, VM_ERROR_SENTINEL);
    let err = vm.last_error().expect("last_error on remote_call failure");
    assert_eq!(err.kind, 6);
    assert_eq!(err.cause, "policy denied");
}

#[test]
fn doc_remote_call_with_no_dispatcher_attached_halts_vm() {
    let src = r#"
        function start() -> str {
            return remote_call("p", "m", "a");
        }
    "#;
    let (v, vm) = run(src);
    assert_eq!(v, VM_ERROR_SENTINEL);
    let err = vm.last_error().expect("last_error when no dispatcher");
    assert!(
        err.cause.contains("no RemoteCallDispatcher"),
        "{}",
        err.cause
    );
}

// ─────────────── §8 try / catch / rethrow ────────────────────

#[test]
fn doc_try_catch_any_swallows_failure_with_fallback_string() {
    // Dispatcher errors; the catch any clause produces a
    // string the flow returns.
    let src = r#"
        function start() -> str {
            try {
                return remote_call("ai", "ai.chat", "x");
            } catch any {
                return "fallback";
            }
        }
    "#;
    let (v, vm) = run_with(src, ErrDispatcher::new(6, "boom"));
    assert_str(&vm, v, "fallback");
}

#[test]
fn doc_try_catch_specific_kind_runs_when_kind_matches() {
    // Dispatcher returns POLICY_DENIED (kind = 6 from error_kinds).
    // The matching `catch policy_denied` runs and returns
    // "denied". `error_kind()` inside the catch reports
    // "policy_denied".
    let src = r#"
        function start() -> str {
            try {
                return remote_call("ai", "ai.chat", "x");
            } catch policy_denied {
                return "denied:" + error_kind();
            } catch any {
                return "other:" + error_kind();
            }
        }
    "#;
    let kind_policy_denied = relix_core::types::error_kinds::POLICY_DENIED;
    let (v, vm) = run_with(src, ErrDispatcher::new(kind_policy_denied, "boom"));
    assert_str(&vm, v, "denied:policy_denied");
}

#[test]
fn doc_try_catch_wrong_kind_no_any_propagates_to_vm_sentinel() {
    // Dispatcher returns POLICY_DENIED; catch only handles
    // timeout. No `catch any`. The synthesised Rethrow at the
    // tail propagates the error; with no outer try, VM halts.
    let src = r#"
        function start() -> str {
            try {
                return remote_call("ai", "ai.chat", "x");
            } catch timeout {
                return "timed_out";
            }
        }
    "#;
    let kind_policy_denied = relix_core::types::error_kinds::POLICY_DENIED;
    let (v, vm) = run_with(src, ErrDispatcher::new(kind_policy_denied, "boom"));
    assert_eq!(v, VM_ERROR_SENTINEL);
    let err = vm
        .last_error()
        .expect("last_error preserved after no-match");
    assert_eq!(err.kind, kind_policy_denied);
}

#[test]
fn doc_rethrow_propagates_to_outer_try() {
    // Inner try catches with `any` then rethrows; outer try
    // catches and returns a sentinel string. Demonstrates
    // nesting + rethrow.
    let src = r#"
        function start() -> str {
            try {
                try {
                    return remote_call("ai", "ai.chat", "x");
                } catch any {
                    rethrow;
                }
            } catch any {
                return "outer-caught:" + error_cause();
            }
        }
    "#;
    let (v, vm) = run_with(src, ErrDispatcher::new(6, "inner-cause"));
    assert_str(&vm, v, "outer-caught:inner-cause");
}

#[test]
fn doc_error_kind_timeout_classification() {
    // TIMEOUT kind maps to "timeout".
    let src = r#"
        function start() -> str {
            try {
                return remote_call("ai", "ai.chat", "x");
            } catch any {
                return error_kind();
            }
        }
    "#;
    let kind_timeout = relix_core::types::error_kinds::TIMEOUT;
    let (v, vm) = run_with(src, ErrDispatcher::new(kind_timeout, "slow"));
    assert_str(&vm, v, "timeout");
}

#[test]
fn doc_error_kind_mesh_classification_for_transport_kind() {
    // TRANSPORT kind maps to "mesh_error".
    let src = r#"
        function start() -> str {
            try {
                return remote_call("ai", "ai.chat", "x");
            } catch any {
                return error_kind();
            }
        }
    "#;
    let kind_transport = relix_core::types::error_kinds::TRANSPORT;
    let (v, vm) = run_with(src, ErrDispatcher::new(kind_transport, "dropped"));
    assert_str(&vm, v, "mesh_error");
}

#[test]
fn doc_error_kind_responder_classification_for_unknown_kind() {
    // An unmapped kind maps to "responder_error".
    let src = r#"
        function start() -> str {
            try {
                return remote_call("ai", "ai.chat", "x");
            } catch any {
                return error_kind();
            }
        }
    "#;
    // 999 is not in the TIMEOUT / TRANSPORT / POLICY_DENIED
    // classifications, so it falls through to responder_error.
    let (v, vm) = run_with(src, ErrDispatcher::new(999, "bad"));
    assert_str(&vm, v, "responder_error");
}

#[test]
fn doc_error_cause_carries_dispatcher_cause_verbatim() {
    let src = r#"
        function start() -> str {
            try {
                return remote_call("ai", "ai.chat", "x");
            } catch any {
                return error_cause();
            }
        }
    "#;
    let (v, vm) = run_with(src, ErrDispatcher::new(6, "literal-cause-text"));
    assert_str(&vm, v, "literal-cause-text");
}

#[test]
fn doc_error_retry_hint_returns_zero_today() {
    // The dispatcher's RemoteCallError does not carry a
    // retry_hint field, so the VM hands back 0.
    let src = r#"
        function start() -> int {
            try {
                let r: str = remote_call("ai", "ai.chat", "x");
                return 1;
            } catch any {
                return error_retry_hint();
            }
        }
    "#;
    let (v, _) = run_with(src, ErrDispatcher::new(6, "boom"));
    assert_eq!(v, 0);
}

#[test]
fn doc_try_recovers_list_get_list_failure() {
    // list_get_list on a string element halts the VM. With a
    // surrounding try { ... } catch any { ... }, the catch runs.
    let src = r#"
        function start() -> str {
            try {
                let xs: list = ["scalar"];
                let inner: list = list_get_list(xs, 0);
                return "unreachable";
            } catch any {
                return "caught:" + error_cause();
            }
        }
    "#;
    let (v, vm) = run(src);
    let s = vm.heap_string(v).expect("heap string at exit");
    assert!(s.starts_with("caught:"), "got {s:?}");
    assert!(s.contains("list_get_list"), "got {s:?}");
}

// ──────────────────── §11.2 Entry point ──────────────────────

#[test]
fn doc_program_with_no_start_function_compiles_and_runs_quietly() {
    // A file with helpers but no `start` compiles. The
    // synthesised final `Call(start_addr, 0)` is omitted (no
    // such addr), so the VM walks off the program end and
    // returns whatever was last on the stack — for a program
    // with zero user statements, that's 0.
    let src = r#"
        function helper() -> int {
            return 1;
        }
    "#;
    let (v, _) = run(src);
    assert_eq!(v, 0);
}

// ───────────────── §9-10 sugar lowerings ─────────────────────

#[test]
fn doc_delegate_sugar_lowers_to_coord_delegate_spawn() {
    // The sugar form should compile and dispatch to the
    // coord/delegate.spawn capability. We use a stub OkDispatcher
    // that returns a child task id; the stub records the wire
    // payload so we can assert the lowering format
    // (`parent|goal||target|0`).
    let src = r#"
        function start() -> str {
            return delegate goal "fix the thing" from "parent-1" to "agent-bob";
        }
    "#;
    let disp = OkDispatcher::new("child-task-id");
    let (v, vm) = run_with(src, disp.clone());
    assert_str(&vm, v, "child-task-id");
    let calls = disp.log.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "delegate must dispatch exactly once");
    assert_eq!(calls[0].0, "coord");
    assert_eq!(calls[0].1, "delegate.spawn");
    assert_eq!(calls[0].2, b"parent-1|fix the thing||agent-bob|0");
}

#[test]
fn doc_send_sugar_lowers_to_coord_msg_send() {
    let src = r#"
        function start() -> str {
            return send subject "hello" body "world" from "alice" to "bob";
        }
    "#;
    let disp = OkDispatcher::new("msg-id");
    let (v, vm) = run_with(src, disp.clone());
    assert_str(&vm, v, "msg-id");
    let calls = disp.log.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "send must dispatch exactly once");
    assert_eq!(calls[0].0, "coord");
    assert_eq!(calls[0].1, "msg.send");
    assert_eq!(calls[0].2, b"alice|bob|hello|world|||0|sol_flow");
}
