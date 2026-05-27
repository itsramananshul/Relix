//! Spot-checks that every key code example in
//! `docs/yaml-flow-reference.md` compiles through
//! `compile_source`. If you edit the doc, edit these in
//! lockstep — the doc carries an implicit promise that the
//! examples work as written, and these tests pin that
//! promise.

#[test]
fn doc_native_list_literal_compiles() {
    let yaml = r#"
        steps:
          - let:
              name: items
              type: list
              value:
                - alpha
                - beta
                - gamma
    "#;
    relix_runtime::yaml_flow::compile_source(yaml).expect("native list literal");
}

#[test]
fn doc_nested_list_of_lists_compiles() {
    let yaml = r#"
        steps:
          - let:
              name: pairs
              type: list
              value:
                - - a
                  - b
                - - c
                  - d
    "#;
    relix_runtime::yaml_flow::compile_source(yaml).expect("nested list");
}

#[test]
fn doc_native_map_literal_compiles() {
    let yaml = r#"
        steps:
          - let:
              name: config
              type: map
              value:
                model: gpt-4o
                temp: "0.2"
    "#;
    relix_runtime::yaml_flow::compile_source(yaml).expect("native map literal");
}

#[test]
fn doc_nested_map_of_maps_compiles() {
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
    relix_runtime::yaml_flow::compile_source(yaml).expect("nested map");
}

#[test]
fn doc_multi_catch_try_compiles() {
    // Verbatim from the `try` section's multi-catch example.
    let yaml = r#"
        steps:
          - let: { name: session, type: str, value: "demo-session" }
          - let: { name: message, type: str, value: "hello" }
          - let: { name: reply, type: str, value: "" }
          - try:
              steps:
                - call:
                    peer: ai
                    method: ai.chat
                    arg: "{{session}}|{{message}}|"
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
                        value: "error"
          - result: "{{reply}}"
    "#;
    relix_runtime::yaml_flow::compile_source(yaml).expect("multi-catch try");
}

#[test]
fn doc_single_catch_shorthand_compiles() {
    // Verbatim from the single-catch shorthand example.
    let yaml = r#"
        steps:
          - let: { name: reply, type: str, value: "" }
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
    relix_runtime::yaml_flow::compile_source(yaml).expect("single-catch shorthand");
}

#[test]
fn doc_curl_example_yaml_compiles_through_validate_route() {
    // The curl example shows YAML that's expected to FAIL
    // schema validation (missing required `value`). It should
    // produce a Semantic error with line/column — not an OK.
    let yaml = "steps:\n  - let:\n      name: x\n      type: str\n";
    let err = relix_runtime::yaml_flow::compile_source(yaml).expect_err("expected failure");
    match err {
        relix_runtime::yaml_flow::YamlFlowError::Semantic {
            ref message,
            line,
            column,
            ..
        } => {
            assert!(
                message.contains("value"),
                "expected missing-field message naming `value`, got: {message}"
            );
            assert!(line > 0, "expected positive line, got {line}");
            assert!(column > 0, "expected positive column, got {column}");
        }
        other => panic!("expected Semantic error, got {other:?}"),
    }
}

#[test]
fn doc_minimum_viable_flow_compiles() {
    // The "minimum viable flow" at the top of the doc.
    let yaml = r#"
        steps:
          - call:
              peer: ai
              method: ai.chat
              arg: "demo|hello|"
              assign: reply
          - result: "{{reply}}"
    "#;
    relix_runtime::yaml_flow::compile_source(yaml).expect("minimum viable flow");
}

#[test]
fn doc_scalar_examples_compile_individually() {
    // The scalar-values sub-section shows three inline lets;
    // run each through the compiler to confirm the YAML
    // syntax is accurate.
    for src in [
        r#"steps: [{let: {name: greeting, type: str, value: "hello"}}]"#,
        r#"steps: [{let: {name: count, type: int, value: 5}}]"#,
        r#"steps: [{let: {name: ok, type: bool, value: true}}]"#,
    ] {
        relix_runtime::yaml_flow::compile_source(src)
            .unwrap_or_else(|e| panic!("scalar example failed to compile: {e}\nsrc: {src}"));
    }
}
