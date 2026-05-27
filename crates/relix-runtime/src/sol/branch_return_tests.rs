//! Branch return-type checker tests.
//!
//! The analyzer's `DeclFunc` handler validates two things
//! against the function signature's declared return type:
//!
//!   1. Every `return` statement — at any nesting depth —
//!      produces a value matching the declared type.
//!   2. The function body guarantees a return on every static
//!      control-flow path (`Void` functions are exempt).
//!
//! These tests pin both contracts. Each test compiles a small
//! source through `crate::sol::compile_source` and asserts
//! either success or the expected error message shape.

#![cfg(test)]

use crate::sol::compile_source;

fn expect_compile_error(source: &str, fragment: &str) {
    match compile_source(source) {
        Ok(_) => panic!(
            "expected compile failure containing `{fragment}` but got success for source:\n{source}"
        ),
        Err(e) => assert!(
            e.contains(fragment),
            "expected error to contain `{fragment}`, got: {e}\nsource:\n{source}"
        ),
    }
}

fn expect_compile_ok(source: &str) {
    compile_source(source)
        .unwrap_or_else(|e| panic!("expected clean compile but got error: {e}\nsource:\n{source}"));
}

// ──────────────────────────── §wrong return type in a branch ────

#[test]
fn return_int_from_if_branch_when_function_returns_str_is_compile_error() {
    let src = r#"
        function start() -> str {
            if true { return 1; } else { return "b"; }
        }
    "#;
    expect_compile_error(src, "return type mismatch");
}

#[test]
fn return_str_from_else_branch_when_function_returns_int_is_compile_error() {
    let src = r#"
        function start() -> int {
            if true { return 1; } else { return "two"; }
        }
    "#;
    expect_compile_error(src, "return type mismatch");
}

#[test]
fn return_wrong_type_from_while_body_is_compile_error() {
    // Wrong-type returns are caught regardless of whether the
    // surrounding construct guarantees execution. The function
    // also fails the always-returns check, but the wrong-type
    // panic fires first.
    let src = r#"
        function start() -> str {
            while true { return 42; }
            return "fallback";
        }
    "#;
    expect_compile_error(src, "return type mismatch");
}

#[test]
fn return_wrong_type_from_for_body_is_compile_error() {
    let src = r#"
        function start() -> str {
            let xs: list = ["a", "b"];
            for x in xs { return 99; }
            return "fallback";
        }
    "#;
    expect_compile_error(src, "return type mismatch");
}

#[test]
fn return_wrong_type_from_try_body_is_compile_error() {
    let src = r#"
        function start() -> str {
            try {
                return 7;
            } catch any {
                return "caught";
            }
        }
    "#;
    expect_compile_error(src, "return type mismatch");
}

#[test]
fn return_wrong_type_from_catch_body_is_compile_error() {
    let src = r#"
        function start() -> int {
            try {
                return 1;
            } catch any {
                return "boom";
            }
        }
    "#;
    expect_compile_error(src, "return type mismatch");
}

#[test]
fn return_wrong_type_from_deeply_nested_branch_is_compile_error() {
    // `try → catch → while body → return wrong type`. The
    // checker walks into every body so depth doesn't hide
    // the mismatch.
    let src = r#"
        function start() -> str {
            try {
                return "ok";
            } catch any {
                while true { return 5; }
                return "never";
            }
        }
    "#;
    expect_compile_error(src, "return type mismatch");
}

// ──────────────────────────── §correct returns compile clean ────

#[test]
fn both_branches_return_same_type_compiles_clean() {
    let src = r#"
        function start() -> int {
            if true { return 10; } else { return 20; }
        }
    "#;
    expect_compile_ok(src);
}

#[test]
fn return_inside_while_with_trailing_return_compiles_clean() {
    // `while` doesn't always execute, so the trailing
    // `return` covers the fall-through. Both returns are
    // type-correct.
    let src = r#"
        function start() -> str {
            let i: int = 0;
            while i < 3 {
                return "from-loop";
            }
            return "default";
        }
    "#;
    expect_compile_ok(src);
}

#[test]
fn return_inside_for_with_trailing_return_compiles_clean() {
    let src = r#"
        function start() -> str {
            let xs: list = ["a"];
            for x in xs {
                return x;
            }
            return "fallback";
        }
    "#;
    expect_compile_ok(src);
}

#[test]
fn try_with_both_paths_returning_compiles_clean() {
    let src = r#"
        function start() -> str {
            try {
                return "from-body";
            } catch any {
                return "from-catch";
            }
        }
    "#;
    expect_compile_ok(src);
}

#[test]
fn multi_catch_with_every_clause_returning_compiles_clean() {
    let src = r#"
        function start() -> str {
            try {
                return "ok";
            } catch timeout {
                return "timeout";
            } catch policy_denied {
                return "denied";
            } catch any {
                return "other";
            }
        }
    "#;
    expect_compile_ok(src);
}

#[test]
fn else_if_chain_compiles_clean_when_every_terminal_branch_returns() {
    let src = r#"
        function start() -> int {
            let x: int = 2;
            if x == 1 { return 10; }
            else if x == 2 { return 20; }
            else { return 30; }
        }
    "#;
    expect_compile_ok(src);
}

// ──────────────────────────── §missing return on some path ───────

#[test]
fn function_with_no_return_and_str_signature_is_compile_error() {
    let src = r#"
        function start() -> str {
            let x: int = 1;
        }
    "#;
    expect_compile_error(src, "does not guarantee a return");
}

#[test]
fn function_with_only_if_branch_returning_is_compile_error() {
    // `if cond { return ... }` without an `else` lets the
    // condition-false path fall through unreturned.
    let src = r#"
        function start() -> str {
            if true { return "a"; }
        }
    "#;
    expect_compile_error(src, "does not guarantee a return");
}

#[test]
fn function_returning_only_from_while_body_is_compile_error() {
    // `while` may execute zero iterations, so the return
    // inside isn't a guarantee.
    let src = r#"
        function start() -> str {
            while true { return "loop"; }
        }
    "#;
    expect_compile_error(src, "does not guarantee a return");
}

#[test]
fn function_returning_only_from_for_body_is_compile_error() {
    let src = r#"
        function start() -> str {
            let xs: list = ["a"];
            for x in xs { return x; }
        }
    "#;
    expect_compile_error(src, "does not guarantee a return");
}

#[test]
fn try_where_body_returns_but_catch_does_not_is_compile_error() {
    let src = r#"
        function start() -> str {
            try {
                return "ok";
            } catch any {
                let n: int = 0;
            }
        }
    "#;
    expect_compile_error(src, "does not guarantee a return");
}

#[test]
fn try_where_one_catch_does_not_return_is_compile_error() {
    let src = r#"
        function start() -> str {
            try {
                return "ok";
            } catch timeout {
                return "timed-out";
            } catch any {
                let n: int = 0;
            }
        }
    "#;
    expect_compile_error(src, "does not guarantee a return");
}

// ──────────────────────────── §void functions skip the check ────

#[test]
fn void_function_with_no_return_compiles_clean() {
    let src = r#"
        function start() {
            let x: int = 1;
        }
    "#;
    expect_compile_ok(src);
}

#[test]
fn void_function_with_print_and_no_return_compiles_clean() {
    let src = r#"
        function start() {
            print("hello");
        }
    "#;
    expect_compile_ok(src);
}

#[test]
fn void_function_with_only_if_returning_compiles_clean() {
    let src = r#"
        function start() {
            if true { return; }
        }
    "#;
    expect_compile_ok(src);
}

// ──────────────────────────── §existing programs still compile ───

#[test]
fn typical_chat_template_shape_with_top_level_return_compiles_clean() {
    // Mirrors the shape of `flows/chat_template.sol` post
    // substitution. Several intermediate expression
    // statements followed by a guaranteed return.
    let src = r#"
        function start() -> str {
            let user_msg: str = "hello";
            let reply: str = "world";
            return reply;
        }
    "#;
    expect_compile_ok(src);
}

#[test]
fn rethrow_in_catch_body_treated_as_diverging() {
    // `rethrow;` re-raises and never falls through. The
    // catch path that ends in `rethrow` counts as "exits
    // the function" for coverage. Combined with a body
    // that returns and an `any` catch that returns, the
    // function still always exits via a return-or-rethrow.
    let src = r#"
        function start() -> str {
            try {
                return "ok";
            } catch timeout {
                rethrow;
            } catch any {
                return "other";
            }
        }
    "#;
    expect_compile_ok(src);
}
