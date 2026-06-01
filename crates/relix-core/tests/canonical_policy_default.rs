//! Canonical-policy contract tests.
//!
//! The audit-2 finding was that operators copying the canonical
//! policy files into a working deployment got a system where
//! whole capability families (`task.*`, `cron.*`, `agent.*`,
//! `msg.*`, `coord.*`, `node.manifest`, …) were silently denied
//! because the policy files omitted them.
//!
//! These tests load `configs/policies/default.toml` directly (via
//! `include_str!`) and exercise the `PolicyEngine::evaluate`
//! path for every capability family that the runtime registers.
//! A regression that drops a rule from default.toml — or that
//! adds a new capability without a corresponding policy rule —
//! fails the build before it can hit a deployed operator.

use relix_core::identity::VerifiedIdentity;
use relix_core::policy::{Decision, PolicyEngine};
use relix_core::types::NodeId;

const DEFAULT_POLICY: &str = include_str!("../../../configs/policies/default.toml");
const AI_POLICY: &str = include_str!("../../../configs/policies/ai.toml");
const MEMORY_POLICY: &str = include_str!("../../../configs/policies/memory.toml");
const TOOL_POLICY: &str = include_str!("../../../configs/policies/tool.toml");
const WEB_BRIDGE_POLICY: &str = include_str!("../../../configs/policies/web-bridge.toml");
// `configs/policies/local.toml` is gitignored (it's an
// operator's per-deployment override — see .gitignore line 50).
// We do NOT include it here because a fresh `git clone`
// would not have the file on disk and `include_str!` would
// fail to compile.

fn id(name: &str, groups: &[&str]) -> VerifiedIdentity {
    VerifiedIdentity {
        subject_id: NodeId::from_pubkey(name.as_bytes()),
        name: name.into(),
        org_id: NodeId::from_pubkey(b"org"),
        groups: groups.iter().map(|s| (*s).into()).collect(),
        role: "agent".into(),
        clearance: "internal".into(),
        bundle_id: [0u8; 32],
    }
}

fn assert_admits(toml_src: &str, label: &str, caller: &VerifiedIdentity, method: &str) {
    let engine = PolicyEngine::from_toml(toml_src).unwrap_or_else(|e| panic!("{label} parse: {e}"));
    match engine.evaluate(caller, method) {
        Decision::Allow { matched_rule } => {
            assert!(
                !matched_rule.is_empty(),
                "{label} admitted `{method}` with empty matched_rule"
            );
        }
        Decision::Deny { reason, .. } => panic!(
            "{label} should admit `{method}` for caller `{}` groups={:?} — got deny: {reason}",
            caller.name, caller.groups
        ),
    }
}

fn assert_denies(toml_src: &str, label: &str, caller: &VerifiedIdentity, method: &str) {
    let engine = PolicyEngine::from_toml(toml_src).unwrap_or_else(|e| panic!("{label} parse: {e}"));
    match engine.evaluate(caller, method) {
        Decision::Deny { .. } => {}
        Decision::Allow { matched_rule } => panic!(
            "{label} should DENY `{method}` for caller `{}` groups={:?} — got allow (rule={matched_rule})",
            caller.name, caller.groups
        ),
    }
}

// ─────────────────────────────────────────────────────────────
// Audit-2 contract tests against configs/policies/default.toml.
// Each test from the prompt is a paragraph below.
// ─────────────────────────────────────────────────────────────

#[test]
fn default_admits_ai_chat_for_chat_users() {
    // "A deployment using only configs/policies/default.toml
    // can successfully call ai.chat".
    let caller = id("alice", &["chat-users"]);
    assert_admits(DEFAULT_POLICY, "default", &caller, "ai.chat");
}

#[test]
fn default_admits_memory_search_for_chat_users() {
    // "A deployment using only configs/policies/default.toml
    // can successfully call memory.search".
    let caller = id("alice", &["chat-users"]);
    assert_admits(DEFAULT_POLICY, "default", &caller, "memory.search");
}

#[test]
fn default_admits_tool_web_read_for_chat_users() {
    // "A deployment using only configs/policies/default.toml
    // can successfully call tool.web_read".
    let caller = id("alice", &["chat-users"]);
    assert_admits(DEFAULT_POLICY, "default", &caller, "tool.web_read");
}

#[test]
fn default_admits_coord_approval_get_for_chat_users() {
    // "A deployment using only configs/policies/default.toml
    // can successfully call coord.approval.get".
    let caller = id("alice", &["chat-users"]);
    assert_admits(DEFAULT_POLICY, "default", &caller, "coord.approval.get");
}

// ─────────────────────────────────────────────────────────────
// Coverage tests: every capability family appears in
// default.toml. The list mirrors `bridge.register("…")` call
// sites in `crates/relix-runtime/src/`; a new capability
// added without a policy rule fails this test.
// ─────────────────────────────────────────────────────────────

/// Every method enumerated below is admitted by default.toml
/// for SOME caller (operators always work since they're the
/// broadest group). Treat this list as the canonical source
/// of truth for "every capability the runtime ships".
const CAPABILITIES_OPERATORS_MUST_ADMIT: &[&str] = &[
    // node.*
    "node.health",
    "node.manifest",
    "node.dispatch.stats",
    "node.policy.simulate",
    "node.policy.recent_denials",
    "node.policy.tenant_list",
    "node.policy.tenant_get",
    "node.audit.tenant_list",
    "node.audit.tenant_recent",
    // ai.*
    "ai.chat",
    "ai.embed",
    "ai.perception_extract",
    // memory.*
    "memory.recent_for_session",
    "memory.write_turn",
    "memory.search",
    "memory.search_turns",
    "memory.session_search",
    "memory.records_search",
    "memory.agent_read",
    "memory.agent_write",
    "memory.agent_curate",
    "memory.curator_status",
    "memory.embed",
    "memory.embed_all",
    "memory.dialectic",
    "memory.ingest_document",
    "memory.ingest_image",
    "memory.context_flush",
    "memory.edit_record",
    "memory.freeze_record",
    "memory.unfreeze_record",
    "memory.quarantine_list",
    "memory.quarantine_approve",
    "memory.quarantine_reject",
    "memory.bulk_export",
    "memory.request_model_refresh",
    "memory.pii_scan",
    "memory.anonymize_preview",
    "memory.bulk_anonymize",
    "memory.skill_store",
    "memory.skill_get",
    "memory.skill_search",
    "memory.skill_stats",
    "memory.skill_update",
    "memory.skill_deprecate",
    // knowledge.*
    "knowledge.share",
    "knowledge.accept_shared",
    "knowledge.group_broadcast",
    "knowledge.groups",
    "knowledge.list_shared",
    "knowledge.recall",
    "knowledge.revoke",
    "knowledge.autoshare_stats",
    // tool.*
    "tool.read_file",
    "tool.write_file",
    "tool.append_file",
    "tool.list_dir",
    "tool.search_files",
    "tool.patch",
    "tool.patch_preview",
    "tool.fuzzy_replace",
    "tool.binary_sniff",
    "tool.pdf",
    "tool.parse_document",
    "tool.text.chunk",
    "tool.screen",
    "tool.fs.stat",
    "tool.fs.tree",
    "tool.fs.audit_recent",
    "tool.web_fetch",
    "tool.web_get",
    "tool.web_read",
    "tool.web_extract",
    "tool.web_search",
    "tool.web.post",
    "tool.web.robots_check",
    "tool.web.blocklist_summary",
    "tool.browser.open_session",
    "tool.browser.close_session",
    "tool.browser.list_sessions",
    "tool.browser.navigate",
    "tool.browser.get_text",
    "tool.browser.click",
    "tool.browser.type_text",
    "tool.browser.wait_for_selector",
    "tool.browser.screenshot",
    "tool.browser.capture_read",
    "tool.terminal.run",
    "tool.terminal.spawn",
    "tool.terminal.cancel",
    "tool.terminal.sessions",
    "tool.terminal.tail",
    "tool.terminal.audit_recent",
    "tool.terminal.shell.open",
    "tool.terminal.shell.close",
    "tool.terminal.shell.input",
    "tool.terminal.shell.control",
    "tool.mcp.list_servers",
    "tool.mcp.list_tools",
    "tool.mcp.invoke",
    // task.*
    "task.create",
    "task.update",
    "task.get",
    "task.list",
    "task.list_cursor",
    "task.count",
    "task.event",
    "task.events",
    "task.recent_events",
    "task.compact_events",
    "task.attempts",
    "task.edges",
    "task.recent_edges",
    "task.lineage",
    "task.subtree_metrics",
    "task.export",
    "task.session_export",
    "task.session_search",
    "task.recover",
    "task.retry",
    "task.replay",
    "task.pause",
    "task.resume",
    "task.freeze",
    "task.unfreeze",
    "task.note",
    "task.mark_investigation",
    "task.stuck",
    "task.interruption_check",
    "task.transition_check",
    "task.observe_interruption",
    "task.record_awaited",
    "task.record_delegated",
    "task.record_spawned",
    "task.todo_list",
    "task.todo_set",
    "task.todo_update",
    // cron.*
    "cron.create",
    "cron.list",
    "cron.get",
    "cron.update",
    "cron.delete",
    "cron.trigger",
    // delegate.*
    "delegate.spawn",
    "delegate.result",
    "delegate.cancel",
    "delegate.list",
    // agent.*
    "agent.create",
    "agent.get",
    "agent.list",
    "agent.update",
    "agent.delete",
    "agent.effective_capabilities",
    "agent.standing_approval.create",
    "agent.standing_approval.list",
    "agent.standing_approval.revoke",
    // coord.* + approval.*
    "coord.approval.pending",
    "coord.approval.get",
    "coord.approval.decide",
    "approval.deliver",
    "approval.delivery_status",
    "approval.failed_deliveries",
    "approval.list_pending",
    "approval.record_decision",
    // msg.*
    "msg.send",
    "msg.inbox",
    "msg.read",
    "msg.thread",
    "msg.delete",
    // planning.*
    "planning.create_plan",
    "planning.validate_spec",
    "planning.list_agents",
    "planning.find_agents",
    "planning.orchestrator_status",
    "planning.approve_plan",
    "planning.reject_plan",
    "planning.list_approvals",
    "planning.get_approval",
    "planning.verification_log",
    "planning.export_spec",
    // workflow.*
    "workflow.list",
    "workflow.run",
    "workflow.status",
    "workflow.validate",
    "workflow.reload",
    // credentials.*
    "credentials.store",
    "credentials.get",
    "credentials.list",
    "credentials.rotate",
    "credentials.revoke",
    "credentials.audit",
    // identity.*
    "identity.issue_token",
    "identity.verify_token",
    "identity.revoke_token",
    "identity.active_tokens",
    "identity.research",
    // routing.*
    "routing.explain",
    "routing.list",
    "routing.resolve",
    // belief.*
    "belief.get",
    "belief.reset",
    // judge.* + reasoning.*
    "judge.recent_verdicts",
    "judge.stats",
    "reasoning.status",
    // confidence.*
    "confidence.policy_list",
    "confidence.score_history",
    "confidence.reset_history",
    "confidence.self_consistency_stats",
    // execution.*
    "execution.rollback",
    "execution.transaction_get",
    "execution.evidence",
    // metrics.*
    "metrics.agents",
    "metrics.agent_summary",
    "metrics.method_breakdown",
    "metrics.timeseries",
    "metrics.alerts_active",
    "metrics.cost_report",
    "metrics.cost_baselines",
    "metrics.cost_spike_history",
    "metrics.ask_human_baselines",
    // observability.* + pii.* + budget.*
    "observability.active_alerts",
    "observability.alert_history",
    "observability.health_summary",
    "pii.scan_stats",
    "pii.recent_events",
    "budget.status",
    "budget.reset",
    // training.*
    "training.list_interactions",
    "training.get_interaction",
    "training.delete_interaction",
    "training.export",
    "training.score_interaction",
    "training.stats",
    "training.pii_scan",
    "training.anonymize_preview",
    // telegram / discord / slack / email
    "telegram.status",
    "telegram.messages_recent",
    "telegram.send",
    "telegram.approval_send",
    "telegram.webhook_update",
    "discord.status",
    "discord.messages_recent",
    "discord.send",
    "discord.approval_send",
    "slack.status",
    "slack.messages_recent",
    "slack.send",
    "slack.approval_send",
    "email.send",
    "email.send_template",
    "email.status",
    "email.messages_recent",
    "email.approval_send",
    // plugins
    "plugin.list",
    "plugin.status",
    "plugin.reload",
    "plugin.disable",
    "plugin_host.plugin.list",
    "plugin_host.plugin.status",
    "plugin_host.plugin.reload",
    "plugin_host.plugin.disable",
    // example plugins
    "hello.greet",
    "plugin_host.hello.greet",
    "web_lookup.fetch",
    "plugin_host.web_lookup.fetch",
];

#[test]
fn default_admits_every_capability_for_operators() {
    let caller = id("ops", &["operators"]);
    let engine = PolicyEngine::from_toml(DEFAULT_POLICY).expect("parse default.toml");
    let mut missing: Vec<&'static str> = Vec::new();
    for method in CAPABILITIES_OPERATORS_MUST_ADMIT {
        match engine.evaluate(&caller, method) {
            Decision::Allow { .. } => {}
            Decision::Deny { .. } => missing.push(method),
        }
    }
    assert!(
        missing.is_empty(),
        "default.toml is missing operator-admitted rules for: {missing:?}"
    );
}

#[test]
fn default_admits_every_capability_for_chat_users() {
    // chat-users is the default mesh-up group. The canonical
    // default.toml must admit every read + standard-write
    // capability for chat-users so a copy-paste local mesh
    // works without operator edits. Admin-only capabilities
    // (e.g. agent.create, memory.bulk_export) are EXCLUDED
    // from this list — they're the ones default.toml
    // restricts to `["operators"]` per the security model.
    let caller = id("alice", &["chat-users"]);
    let engine = PolicyEngine::from_toml(DEFAULT_POLICY).expect("parse default.toml");
    let mut missing: Vec<&'static str> = Vec::new();
    for method in CAPABILITIES_CHAT_USERS_MUST_ADMIT {
        match engine.evaluate(&caller, method) {
            Decision::Allow { .. } => {}
            Decision::Deny { .. } => missing.push(method),
        }
    }
    assert!(
        missing.is_empty(),
        "default.toml is missing chat-users-admitted rules for: {missing:?}"
    );
}

/// The chat-users subset of [`CAPABILITIES_OPERATORS_MUST_ADMIT`]
/// — everything the default group needs for a working local
/// deployment. Admin-only verbs (creates, deletes, bulk
/// exports, scheduler manipulation, hard policy reads) live
/// behind `operators` in default.toml and are not listed here.
const CAPABILITIES_CHAT_USERS_MUST_ADMIT: &[&str] = &[
    // node.* discovery
    "node.health",
    "node.manifest",
    // ai.*
    "ai.chat",
    "ai.embed",
    "ai.perception_extract",
    // memory.* read + standard write + skill registry
    "memory.recent_for_session",
    "memory.write_turn",
    "memory.search",
    "memory.search_turns",
    "memory.session_search",
    "memory.records_search",
    "memory.agent_read",
    "memory.agent_write",
    "memory.agent_curate",
    "memory.curator_status",
    "memory.embed",
    "memory.dialectic",
    "memory.ingest_document",
    "memory.ingest_image",
    "memory.context_flush",
    "memory.skill_store",
    "memory.skill_get",
    "memory.skill_search",
    "memory.skill_stats",
    "memory.skill_update",
    // knowledge.*
    "knowledge.share",
    "knowledge.accept_shared",
    "knowledge.group_broadcast",
    "knowledge.groups",
    "knowledge.list_shared",
    "knowledge.recall",
    // tool.* read / fetch / patch / browser / mcp
    "tool.read_file",
    "tool.write_file",
    "tool.append_file",
    "tool.list_dir",
    "tool.search_files",
    "tool.patch",
    "tool.patch_preview",
    "tool.fuzzy_replace",
    "tool.binary_sniff",
    "tool.pdf",
    "tool.parse_document",
    "tool.text.chunk",
    "tool.screen",
    "tool.fs.stat",
    "tool.fs.tree",
    "tool.web_fetch",
    "tool.web_get",
    "tool.web_read",
    "tool.web_extract",
    "tool.web_search",
    "tool.web.post",
    "tool.web.robots_check",
    "tool.browser.open_session",
    "tool.browser.close_session",
    "tool.browser.list_sessions",
    "tool.browser.navigate",
    "tool.browser.get_text",
    "tool.browser.click",
    "tool.browser.type_text",
    "tool.browser.wait_for_selector",
    "tool.browser.screenshot",
    "tool.browser.capture_read",
    "tool.mcp.list_servers",
    "tool.mcp.list_tools",
    "tool.mcp.invoke",
    // task.* — the lifecycle surface chat-users drive
    "task.create",
    "task.update",
    "task.get",
    "task.list",
    "task.list_cursor",
    "task.count",
    "task.event",
    "task.events",
    "task.recent_events",
    "task.attempts",
    "task.edges",
    "task.recent_edges",
    "task.lineage",
    "task.subtree_metrics",
    "task.export",
    "task.session_export",
    "task.session_search",
    "task.retry",
    "task.replay",
    "task.pause",
    "task.resume",
    "task.note",
    "task.record_awaited",
    "task.record_delegated",
    "task.record_spawned",
    "task.todo_list",
    "task.todo_set",
    "task.todo_update",
    // cron.* (delete is operator-only)
    "cron.create",
    "cron.list",
    "cron.get",
    "cron.update",
    "cron.trigger",
    // delegate.*
    "delegate.spawn",
    "delegate.result",
    "delegate.cancel",
    "delegate.list",
    // agent.* read + standing-approval read
    "agent.get",
    "agent.list",
    "agent.effective_capabilities",
    "agent.standing_approval.list",
    // coord.approval.* + approval.* end-user surface
    "coord.approval.pending",
    "coord.approval.get",
    "coord.approval.decide",
    "approval.delivery_status",
    "approval.list_pending",
    "approval.record_decision",
    // msg.*
    "msg.send",
    "msg.inbox",
    "msg.read",
    "msg.thread",
    "msg.delete",
    // planning.* (approve/reject are operator-only)
    "planning.create_plan",
    "planning.validate_spec",
    "planning.list_agents",
    "planning.find_agents",
    "planning.orchestrator_status",
    "planning.list_approvals",
    "planning.get_approval",
    "planning.verification_log",
    "planning.export_spec",
    // workflow.* (reload is operator-only)
    "workflow.list",
    "workflow.run",
    "workflow.status",
    "workflow.validate",
    // credentials.* read (write/rotate/revoke/audit operator-only)
    "credentials.get",
    "credentials.list",
    // identity.* — verify + research are end-user; the
    // mint/revoke surface is operator-only
    "identity.verify_token",
    "identity.research",
    // routing.* read
    "routing.explain",
    "routing.list",
    "routing.resolve",
    // belief.* (reset operator-only)
    "belief.get",
    // judge.* / reasoning.* reads
    "judge.recent_verdicts",
    "judge.stats",
    "reasoning.status",
    // confidence.* reads (reset operator-only)
    "confidence.policy_list",
    "confidence.score_history",
    "confidence.self_consistency_stats",
    // observability + budget reads
    "observability.health_summary",
    "budget.status",
    // channel surfaces — end-user reads + sends
    "telegram.status",
    "telegram.messages_recent",
    "telegram.send",
    "discord.status",
    "discord.messages_recent",
    "discord.send",
    "slack.status",
    "slack.messages_recent",
    "slack.send",
    "email.send",
    "email.send_template",
    "email.status",
    "email.messages_recent",
    // plugins (read-only; reload/disable operator-only)
    "plugin.list",
    "plugin.status",
    "plugin_host.plugin.list",
    "plugin_host.plugin.status",
    // example plugin caps
    "hello.greet",
    "plugin_host.hello.greet",
    "web_lookup.fetch",
    "plugin_host.web_lookup.fetch",
];

#[test]
fn default_denies_when_caller_has_no_admitted_group() {
    // Even after the comprehensive rule coverage, a caller
    // outside the admit list must NOT slip through (the
    // PolicyEngine's [admit] check fires before per-rule
    // evaluation).
    let caller = id("nobody", &["random-unlisted-group"]);
    assert_denies(DEFAULT_POLICY, "default", &caller, "ai.chat");
    assert_denies(DEFAULT_POLICY, "default", &caller, "node.manifest");
}

// ─────────────────────────────────────────────────────────────
// Per-node policy coverage — each per-node file admits at
// least node.manifest for chat-users so manifest discovery
// works.
// ─────────────────────────────────────────────────────────────

#[test]
fn ai_policy_admits_node_manifest_for_chat_users() {
    let caller = id("alice", &["chat-users"]);
    assert_admits(AI_POLICY, "ai.toml", &caller, "node.manifest");
}

#[test]
fn memory_policy_admits_node_manifest_for_chat_users() {
    let caller = id("alice", &["chat-users"]);
    assert_admits(MEMORY_POLICY, "memory.toml", &caller, "node.manifest");
}

#[test]
fn tool_policy_admits_node_manifest_for_chat_users() {
    let caller = id("alice", &["chat-users"]);
    assert_admits(TOOL_POLICY, "tool.toml", &caller, "node.manifest");
}

#[test]
fn web_bridge_policy_admits_node_manifest_for_chat_users() {
    let caller = id("alice", &["chat-users"]);
    assert_admits(
        WEB_BRIDGE_POLICY,
        "web-bridge.toml",
        &caller,
        "node.manifest",
    );
}

#[test]
fn ai_policy_admits_ai_chat_for_chat_users() {
    let caller = id("alice", &["chat-users"]);
    assert_admits(AI_POLICY, "ai.toml", &caller, "ai.chat");
}

#[test]
fn memory_policy_admits_memory_search_for_chat_users() {
    let caller = id("alice", &["chat-users"]);
    assert_admits(MEMORY_POLICY, "memory.toml", &caller, "memory.search");
}

#[test]
fn tool_policy_admits_tool_web_read_for_chat_users() {
    let caller = id("alice", &["chat-users"]);
    assert_admits(TOOL_POLICY, "tool.toml", &caller, "tool.web_read");
}

// ── SEC §14: web-bridge.toml must admit the operator-facing
// families the bridge proxies. Before the sync this file lacked
// task.*/cron.*/agent.*/coord.*/msg.* rules, so a node loaded
// with it returned 6xx on the task list, approval inbox, agent
// settings, cron jobs, and messaging. ──────────────────────────

#[test]
fn web_bridge_policy_admits_proxied_operator_families_no_6xx() {
    // One representative method per family the dashboard exercises.
    let ops = id("ops", &["operators", "web-bridge-svc"]);
    for method in [
        "task.list",
        "task.create",
        "task.events",
        "cron.list",
        "cron.create",
        "agent.list",
        "agent.get",
        "coord.approval.pending",
        "coord.approval.get",
        "msg.send",
        "msg.inbox",
    ] {
        assert_admits(WEB_BRIDGE_POLICY, "web-bridge.toml", &ops, method);
    }
}

#[test]
fn web_bridge_policy_admits_task_and_approval_for_chat_users() {
    // The common end-user path: a chat-user opening the task list
    // and approval inbox must not be denied.
    let alice = id("alice", &["chat-users"]);
    assert_admits(WEB_BRIDGE_POLICY, "web-bridge.toml", &alice, "task.list");
    assert_admits(
        WEB_BRIDGE_POLICY,
        "web-bridge.toml",
        &alice,
        "coord.approval.pending",
    );
}
