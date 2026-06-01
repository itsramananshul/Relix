//! SEC §14: policy-coverage contract test for the tool node.
//!
//! The audit-2/3 finding was that per-node policy files drifted out
//! of sync with what the node actually registers, so capabilities
//! were silently denied (6xx) on a fresh deployment. This test
//! derives the expected capability set from the ACTUAL registration
//! source — `relix_runtime::nodes::tool::advertised_capabilities`,
//! the same function `register_node_type_handlers` iterates to build
//! the tool node's manifest — and fails if any advertised capability
//! lacks a rule in `configs/policies/tool.toml`. It is NOT a
//! hand-maintained list: add a new tool capability to
//! `advertised_capabilities` without a policy rule and this test goes
//! red before it can reach an operator.

use std::collections::HashSet;

use relix_core::policy::PolicyFile;
use relix_runtime::nodes::tool::{ToolConfig, advertised_capabilities};

const TOOL_POLICY: &str = include_str!("../../../configs/policies/tool.toml");

#[test]
fn tool_policy_covers_every_advertised_capability() {
    let policy: PolicyFile = toml::from_str(TOOL_POLICY).expect("tool.toml parses as a PolicyFile");
    let rule_methods: HashSet<&str> = policy.rules.iter().map(|r| r.method.as_str()).collect();

    // Default tool config → the unconditional advertised set
    // (web_fetch, web_extract, web_get/search/post, blocklist_summary,
    // robots_check, text.chunk, ask_human, memory.session_search).
    let caps = advertised_capabilities(&ToolConfig::default());
    assert!(
        !caps.is_empty(),
        "advertised_capabilities returned nothing — derivation is broken"
    );

    let missing: Vec<String> = caps
        .iter()
        .map(|c| c.method_name.clone())
        .filter(|m| !rule_methods.contains(m.as_str()))
        .collect();

    assert!(
        missing.is_empty(),
        "configs/policies/tool.toml is missing [[rules]] for tool capabilities the node \
         advertises (a registered capability with no policy rule = silent 6xx for operators): {missing:?}"
    );
}

#[test]
fn tool_config_rejects_legacy_max_body_bytes_typo() {
    // SEC §14: `[tool]` now denies unknown fields, so the legacy
    // `max_body_bytes` (parsed by no code — the runtime reads
    // `max_bytes`) is a hard error instead of a silently-dropped
    // SSRF body cap.
    let err = toml::from_str::<ToolConfig>("max_body_bytes = 524288")
        .expect_err("unknown `max_body_bytes` key must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("max_body_bytes") || msg.contains("unknown field"),
        "error should name the offending key: {msg}"
    );

    // The real field still parses.
    let ok = toml::from_str::<ToolConfig>("max_bytes = 524288").expect("max_bytes parses");
    assert_eq!(ok.max_bytes, 524288);
}
