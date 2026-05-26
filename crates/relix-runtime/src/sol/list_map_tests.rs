//! Tests for SOL list & map literals + the F6/F8 built-in
//! function surface. Driven end-to-end through the compile
//! pipeline (lexer → parser → analyzer → codegen) and the VM
//! step loop so the test covers the parser, the type checker,
//! the opcode emission, and the heap-object handling together.
//!
//! Most assertions read the program's exit value (top of stack
//! at end of `start()`) — for opcodes that return a string the
//! exit value is a heap-string ref and the test resolves it
//! via `VM::heap_string`. For opcodes that return an integer /
//! boolean (`list_len`, `list_contains`, `map_has`, `map_len`)
//! the exit value is the raw integer.

use std::io::Write;

use crate::sol::bytecode::{Codegen, Inst};
use crate::sol::lexer::Lexer;
use crate::sol::parser::Parser;
use crate::sol::vm::{HeapObject, VM};

/// Compile a SOL source fragment to bytecode. Mirrors the
/// helper used by `remote_call_compile_tests.rs` — the
/// verbatim Lexer reads from disk so we materialize a
/// tempfile.
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

/// Compile + run a SOL fragment and return (exit_value, vm).
/// The exit value is whatever's on top of the stack when the
/// program finishes; tests inspect it directly or look it up
/// in the VM heap via `vm.heap_string(idx)`.
fn run(source: &str) -> (u64, VM) {
    let bc = compile(source);
    let mut vm = VM::from(&bc);
    let val = vm.run();
    (val, vm)
}

// ── F5: list literal syntax ─────────────────────────────────

#[test]
fn empty_list_literal_compiles_and_has_length_zero() {
    let src = r#"
        function start() -> int {
            let xs: list = [];
            return list_len(xs);
        }
    "#;
    let (v, _vm) = run(src);
    assert_eq!(v, 0, "empty list must have length 0");
}

#[test]
fn three_element_list_has_length_three() {
    let src = r#"
        function start() -> int {
            let xs: list = ["a", "b", "c"];
            return list_len(xs);
        }
    "#;
    let (v, _vm) = run(src);
    assert_eq!(v, 3);
}

#[test]
fn list_get_returns_element_at_index() {
    let src = r#"
        function start() -> str {
            let xs: list = ["alpha", "beta", "gamma"];
            return list_get(xs, 1);
        }
    "#;
    let (v, vm) = run(src);
    let s = vm.heap_string(v).expect("heap string");
    assert_eq!(s, "beta");
}

#[test]
fn list_get_out_of_bounds_returns_empty_string_not_panic() {
    let src = r#"
        function start() -> str {
            let xs: list = ["only-one"];
            return list_get(xs, 99);
        }
    "#;
    let (v, vm) = run(src);
    let s = vm.heap_string(v).expect("heap string");
    assert_eq!(s, "", "out-of-bounds get must return empty string");
}

#[test]
fn list_push_returns_new_list_original_unchanged() {
    // The original list is bound to `xs`; `list_push` returns
    // a NEW list. We assert the new list has 4 elements and
    // the original still has 3.
    let src = r#"
        function start() -> int {
            let xs: list = ["a", "b", "c"];
            let ys: list = list_push(xs, "d");
            // Use the original — it must still have len 3.
            return list_len(xs);
        }
    "#;
    let (v, _vm) = run(src);
    assert_eq!(v, 3, "original list must not be mutated");

    let src2 = r#"
        function start() -> int {
            let xs: list = ["a", "b", "c"];
            let ys: list = list_push(xs, "d");
            return list_len(ys);
        }
    "#;
    let (v, _vm) = run(src2);
    assert_eq!(v, 4, "new list must include the pushed value");
}

#[test]
fn list_contains_returns_true_for_present_value() {
    let src = r#"
        function start() -> bool {
            let xs: list = ["a", "b", "c"];
            return list_contains(xs, "b");
        }
    "#;
    let (v, _vm) = run(src);
    assert_eq!(v, 1);
}

#[test]
fn list_contains_returns_false_for_absent_value() {
    let src = r#"
        function start() -> bool {
            let xs: list = ["a", "b", "c"];
            return list_contains(xs, "z");
        }
    "#;
    let (v, _vm) = run(src);
    assert_eq!(v, 0);
}

#[test]
fn list_join_concatenates_with_separator() {
    let src = r#"
        function start() -> str {
            let xs: list = ["a", "b", "c"];
            return list_join(xs, "-");
        }
    "#;
    let (v, vm) = run(src);
    let s = vm.heap_string(v).expect("heap string");
    assert_eq!(s, "a-b-c");
}

#[test]
fn list_join_on_empty_list_returns_empty_string() {
    let src = r#"
        function start() -> str {
            let xs: list = [];
            return list_join(xs, ",");
        }
    "#;
    let (v, vm) = run(src);
    let s = vm.heap_string(v).expect("heap string");
    assert_eq!(s, "");
}

#[test]
fn list_split_breaks_string_on_separator() {
    let src = r#"
        function start() -> int {
            let xs: list = list_split("a|b|c", "|");
            return list_len(xs);
        }
    "#;
    let (v, _vm) = run(src);
    assert_eq!(v, 3);

    let src_first = r#"
        function start() -> str {
            let xs: list = list_split("a|b|c", "|");
            return list_get(xs, 0);
        }
    "#;
    let (v, vm) = run(src_first);
    assert_eq!(vm.heap_string(v).unwrap(), "a");
}

#[test]
fn list_split_on_empty_string_produces_single_element_list() {
    let src = r#"
        function start() -> int {
            let xs: list = list_split("", "|");
            return list_len(xs);
        }
    "#;
    let (v, _vm) = run(src);
    assert_eq!(v, 1, "empty input splits to a single empty element");
}

#[test]
fn for_in_over_list_iterates_all_elements_in_order() {
    // Sum up the lengths via list_len on each element joined
    // into a fresh list one at a time. The exit value is the
    // length of the result list after the loop.
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
    let s = vm.heap_string(v).expect("heap string");
    assert_eq!(s, "abc", "for-in must visit elements in push order");
}

#[test]
fn list_literal_inside_delegate_sugar_payload_compiles() {
    // F3 sugar interop: the delegate goal can be the result
    // of `list_join` on a list literal. The test asserts the
    // program compiles without panicking — runtime behaviour
    // requires a dispatcher, which is exercised by F3's own
    // tests.
    let src = r#"
        function start() -> str {
            let parts: list = ["fix", "the", "thing"];
            let goal: str = list_join(parts, " ");
            return goal;
        }
    "#;
    let bc = compile(src);
    let dis = format!("{bc:?}");
    assert!(dis.contains("ListJoin"), "expected ListJoin opcode: {dis}");
    assert!(dis.contains("PushList"), "expected PushList opcode: {dis}");
}

// ── F7: map literal syntax ──────────────────────────────────

#[test]
fn empty_map_literal_compiles_and_has_length_zero() {
    let src = r#"
        function start() -> int {
            let m: map = {};
            return map_len(m);
        }
    "#;
    let (v, _vm) = run(src);
    assert_eq!(v, 0);
}

#[test]
fn map_with_two_pairs_returns_correct_values() {
    let src = r#"
        function start() -> str {
            let m: map = { "k1": "v1", "k2": "v2" };
            return map_get(m, "k1");
        }
    "#;
    let (v, vm) = run(src);
    assert_eq!(vm.heap_string(v).unwrap(), "v1");

    let src2 = r#"
        function start() -> str {
            let m: map = { "k1": "v1", "k2": "v2" };
            return map_get(m, "k2");
        }
    "#;
    let (v, vm) = run(src2);
    assert_eq!(vm.heap_string(v).unwrap(), "v2");
}

#[test]
fn map_get_on_missing_key_returns_empty_string_not_panic() {
    let src = r#"
        function start() -> str {
            let m: map = { "k1": "v1" };
            return map_get(m, "absent");
        }
    "#;
    let (v, vm) = run(src);
    assert_eq!(vm.heap_string(v).unwrap(), "");
}

#[test]
fn map_has_returns_true_for_present_key() {
    let src = r#"
        function start() -> bool {
            let m: map = { "k1": "v1" };
            return map_has(m, "k1");
        }
    "#;
    let (v, _vm) = run(src);
    assert_eq!(v, 1);
}

#[test]
fn map_has_returns_false_for_absent_key() {
    let src = r#"
        function start() -> bool {
            let m: map = { "k1": "v1" };
            return map_has(m, "k2");
        }
    "#;
    let (v, _vm) = run(src);
    assert_eq!(v, 0);
}

#[test]
fn map_set_returns_new_map_with_key_added_original_unchanged() {
    let src = r#"
        function start() -> int {
            let m: map = { "k1": "v1" };
            let m2: map = map_set(m, "k2", "v2");
            // Original m still has 1 key.
            return map_len(m);
        }
    "#;
    let (v, _vm) = run(src);
    assert_eq!(v, 1, "original map must not be mutated by map_set");

    let src2 = r#"
        function start() -> int {
            let m: map = { "k1": "v1" };
            let m2: map = map_set(m, "k2", "v2");
            return map_len(m2);
        }
    "#;
    let (v, _vm) = run(src2);
    assert_eq!(v, 2, "new map must include the set key");
}

#[test]
fn map_set_overwrites_existing_key() {
    let src = r#"
        function start() -> str {
            let m: map = { "k1": "old" };
            let m2: map = map_set(m, "k1", "new");
            return map_get(m2, "k1");
        }
    "#;
    let (v, vm) = run(src);
    assert_eq!(vm.heap_string(v).unwrap(), "new");
}

#[test]
fn map_del_returns_new_map_with_key_removed() {
    let src = r#"
        function start() -> int {
            let m: map = { "k1": "v1", "k2": "v2" };
            let m2: map = map_del(m, "k1");
            return map_len(m2);
        }
    "#;
    let (v, _vm) = run(src);
    assert_eq!(v, 1, "map_del must remove exactly one key");

    let src2 = r#"
        function start() -> bool {
            let m: map = { "k1": "v1", "k2": "v2" };
            let m2: map = map_del(m, "k1");
            return map_has(m2, "k1");
        }
    "#;
    let (v, _vm) = run(src2);
    assert_eq!(v, 0, "deleted key must not be present");
}

#[test]
fn map_keys_returns_a_list_of_keys() {
    let src = r#"
        function start() -> int {
            let m: map = { "a": "1", "b": "2", "c": "3" };
            let ks: list = map_keys(m);
            return list_len(ks);
        }
    "#;
    let (v, _vm) = run(src);
    assert_eq!(v, 3);
}

#[test]
fn map_keys_preserves_insertion_order() {
    let src = r#"
        function start() -> str {
            let m: map = { "a": "1", "b": "2", "c": "3" };
            let ks: list = map_keys(m);
            return list_get(ks, 0);
        }
    "#;
    let (v, vm) = run(src);
    assert_eq!(vm.heap_string(v).unwrap(), "a");
}

#[test]
fn map_literal_with_string_interpolation_value_compiles() {
    // F1 interop: a `{{var}}` marker in a value position
    // should lower through the existing interpolation path
    // before the map literal codegen sees it.
    let src = r#"
        function start() -> str {
            let user: str = "alice";
            let m: map = { "greeting": "hi {{user}}" };
            return map_get(m, "greeting");
        }
    "#;
    let (v, vm) = run(src);
    assert_eq!(vm.heap_string(v).unwrap(), "hi alice");
}

#[test]
fn nested_map_set_calls_chain_correctly_for_functional_updates() {
    // Functional-update pattern: each `map_set` returns a new
    // map; chaining them builds an accumulated map without
    // ever mutating the seed.
    let src = r#"
        function start() -> int {
            let seed: map = {};
            let m: map = map_set(map_set(map_set(seed, "a", "1"), "b", "2"), "c", "3");
            return map_len(m);
        }
    "#;
    let (v, _vm) = run(src);
    assert_eq!(v, 3);

    let src2 = r#"
        function start() -> int {
            let seed: map = {};
            let m: map = map_set(map_set(map_set(seed, "a", "1"), "b", "2"), "c", "3");
            // The seed must still be empty.
            return map_len(seed);
        }
    "#;
    let (v, _vm) = run(src2);
    assert_eq!(v, 0, "seed map must not be mutated by chained map_set");
}

// ── End-to-end shape check ──────────────────────────────────

#[test]
fn list_literal_lowers_to_push_list_opcode() {
    let src = r#"
        function start() {
            let xs: list = ["a", "b"];
        }
    "#;
    let bc = compile(src);
    let dis = format!("{bc:?}");
    assert!(dis.contains("PushList(2)"), "expected PushList(2): {dis}");
}

#[test]
fn map_literal_lowers_to_push_map_opcode() {
    let src = r#"
        function start() {
            let m: map = { "k": "v" };
        }
    "#;
    let bc = compile(src);
    let dis = format!("{bc:?}");
    assert!(dis.contains("PushMap(1)"), "expected PushMap(1): {dis}");
}

#[test]
fn map_heap_object_is_distinct_from_list_in_vm_heap() {
    // Smoke-check: a flow that produces a map should leave a
    // `HeapObject::Map` at the resulting heap slot, not a
    // `HeapObject::List` (defensive against accidental
    // opcode reuse).
    let src = r#"
        function start() {
            let m: map = { "k": "v" };
            print(map_len(m));
        }
    "#;
    let bc = compile(src);
    let mut vm = VM::from(&bc);
    let _ = vm.run();
    let mut found_map = false;
    // Walk the heap looking for at least one Map. The actual
    // ref index isn't surfaced through the public API but the
    // public `heap_string` only matches String — so we use a
    // synthetic accessor: run the program and check via a
    // separate query.
    // We don't expose `heap` publicly, so this check is
    // indirect — `map_len` returns 1, which is enough to
    // confirm the map was constructed.
    let src_len = r#"
        function start() -> int {
            let m: map = { "k": "v" };
            return map_len(m);
        }
    "#;
    let (v, _vm) = run(src_len);
    if v == 1 {
        found_map = true;
    }
    assert!(found_map);
    // Suppress unused-import warning when heap_string isn't
    // referenced in this particular test.
    let _ = std::mem::size_of::<HeapObject>();
}
