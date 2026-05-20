//! Mechanical guardrails for the bridge contract.
//!
//! See [`docs/bridge-invariants.md`](../../../docs/bridge-invariants.md)
//! for the full contract. These tests are deliberately minimal —
//! they're canary tests, not full invariant enforcement. The
//! human-review checklist in the doc carries the rest.
//!
//! When one of these fails, the right reaction is NOT to weaken
//! the test; it's to push the responsibility into the Coordinator
//! (state) or SOL (orchestration), or to write down explicitly
//! why the bridge contract needs to change.

use std::path::PathBuf;

/// The bridge MUST NOT depend on rusqlite directly. Persistent
/// task state lives on the Coordinator; the bridge is a stateless
/// HTTP-to-libp2p translation layer.
///
/// This guards against the most common architectural regression:
/// "let me just cache that locally" → operator-visible
/// inconsistency between bridge and Coordinator.
#[test]
fn bridge_has_no_sqlite_dependency() {
    let cargo_toml = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let contents = std::fs::read_to_string(&cargo_toml).expect("read Cargo.toml");
    // The dependency would appear as `rusqlite` somewhere in
    // `[dependencies]` or `[dev-dependencies]`. We tolerate the
    // word inside a comment or doc-comment (none today), and
    // tolerate it appearing inside this very test file path on
    // the off chance — but Cargo.toml is a known-small file so
    // we just check for the bare crate name as a key.
    let banned = [
        "\nrusqlite ",
        "\nrusqlite=",
        "\nrusqlite\t",
        "rusqlite.workspace",
        "rusqlite =",
    ];
    for needle in banned {
        assert!(
            !contents.contains(needle),
            "relix-web-bridge MUST NOT depend on rusqlite (found '{needle}'); \
             persistent state lives on the Coordinator. See \
             docs/bridge-invariants.md."
        );
    }
}

/// The bridge MUST NOT instantiate a `PolicyEngine` of its own.
/// Each responder evaluates its own policy on every inbound RPC;
/// a bridge-side policy decision would be either redundant
/// (with the Coordinator's) or, worse, divergent.
#[test]
fn bridge_does_not_instantiate_policy_engine() {
    let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = Vec::new();
    visit_rs_files(&src_dir, &mut |path, contents| {
        // Match constructor invocations only. Type references in
        // docstrings or comments are tolerated.
        for needle in [
            "PolicyEngine::new(",
            "PolicyEngine::from_path(",
            "PolicyEngine::permissive(",
        ] {
            if contents.contains(needle) {
                hits.push(format!("{}: {needle}", path.display()));
            }
        }
    });
    assert!(
        hits.is_empty(),
        "relix-web-bridge MUST NOT instantiate a PolicyEngine — that's a \
         responder concern. Hits: {hits:?}. See docs/bridge-invariants.md."
    );
}

/// The bridge MUST NOT write to its own EventLog (per-flow logs
/// are written inside `FlowRunner::run` on the runtime crate side,
/// not by the bridge handlers).
#[test]
fn bridge_does_not_open_its_own_event_log() {
    let src_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut hits = Vec::new();
    visit_rs_files(&src_dir, &mut |path, contents| {
        if contents.contains("EventLog::open(") {
            hits.push(path.display().to_string());
        }
    });
    assert!(
        hits.is_empty(),
        "relix-web-bridge MUST NOT open its own EventLog. \
         Per-flow logs are FlowRunner's responsibility. Hits: {hits:?}. \
         See docs/bridge-invariants.md."
    );
}

/// Walk every `.rs` file under `dir` recursively and invoke
/// `visit(path, contents)`. Used by the invariant scans above to
/// keep the check confined to the bridge crate's actual source.
fn visit_rs_files(dir: &PathBuf, visit: &mut impl FnMut(&PathBuf, &str)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_rs_files(&path, visit);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs")
            && let Ok(contents) = std::fs::read_to_string(&path)
        {
            visit(&path, &contents);
        }
    }
}
