//! Agent employee permission gate.
//!
//! Slotted into [`crate::dispatch::DispatchBridge::handle_inbound`]
//! between identity verification (step 5) and the policy
//! engine (step 9). Reads a per-subject [`AgentGateView`] from
//! the [`AgentStore`]; runs the categorical / surface / risk-
//! ceiling / approval checks described in
//! `docs/proposals/agent-employee-permissions.md`.
//!
//! ## Backward compatibility
//!
//! When no agent profile exists for the caller's
//! `subject_id`, the gate returns [`GateDecision::Allow`]
//! unchanged. Existing callers without profiles see today's
//! exact behavior.
//!
//! ## Policy floor
//!
//! Categorical permissions can NEVER widen what the
//! PolicyEngine denies. The gate is **additive narrowing**;
//! it only runs BEFORE the policy engine. If this gate
//! returns `Allow`, the policy engine still gets the final
//! say. Documented in this module's tests
//! (`policy_floor_holds_after_gate_allow`).

use std::sync::Arc;

use relix_core::capability::CapabilityDescriptor;
use relix_core::identity::VerifiedIdentity;

use crate::nodes::coordinator::agent::store::{
    AgentGateView, AgentStore, AgentStoreError, ApprovalRecord, ApprovalStatus,
};
use crate::transport::envelope::RequestEnvelope;

/// What the gate decides about one inbound call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Admit through to the next admission step (policy
    /// engine). Carries a structured outcome the caller can
    /// surface in the audit log.
    Allow(GateAllow),
    /// Deny outright. Caller returns a `POLICY_DENIED`-class
    /// error envelope with `cause = reason`.
    Deny(GateDeny),
    /// The call requires an operator approval. Caller mints
    /// an approval_request row (out of band), writes the
    /// `task.approval_requested` chronicle event, flips the
    /// task to `awaiting_input`, and returns an
    /// `APPROVAL_REQUIRED` error to the agent so it can poll.
    RequireApproval(GateApprovalRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateAllow {
    /// Optional matched rule for audit logging. Empty when
    /// the caller had no agent profile (backward-compat path).
    pub matched_rule: String,
    /// When the call carried a one-shot `approval_token` and
    /// the gate consumed it, this carries the corresponding
    /// approval_id so the audit row can correlate.
    pub consumed_approval_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateDeny {
    pub reason: String,
    pub matched_rule: String,
    /// `agent_id` of the denied caller, when present. Used by
    /// the chronicle / audit writer.
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateApprovalRequest {
    pub agent_id: String,
    pub subject_id: String,
    pub method: String,
    pub category: String,
    pub reason: String,
    pub approver_groups: Vec<String>,
    pub approval_timeout_secs: i64,
    /// Optional task_id the calling agent is acting on. Read
    /// from `RequestEnvelope::task_id` at gate time. The
    /// coordinator-side `on_require_approval` closure stamps
    /// this on the new approval row and flips the task to
    /// `awaiting_input`; `coord.approval.decide` resumes /
    /// fails the same task. `None` when the caller didn't
    /// supply one — the approval row is still created and
    /// can be decided through poll/decide, just without
    /// auto-pausing a task.
    pub task_id: Option<String>,
}

/// Reasons we surface as `matched_rule` for audit + denial
/// rings. Stable string keys callers can search for.
pub mod deny_reasons {
    pub const AGENT_SUSPENDED: &str = "agent_suspended";
    pub const AGENT_DISABLED: &str = "agent_disabled";
    pub const AGENT_SURFACE_DENIED: &str = "agent_surface_denied";
    pub const AGENT_RISK_CEILING_EXCEEDED: &str = "agent_risk_ceiling_exceeded";
    pub const AGENT_CATEGORY_DENIED: &str = "agent_category_denied";
    pub const AGENT_SENSITIVITY_DENIED: &str = "agent_sensitivity_denied";
    pub const AGENT_CATEGORY_NOT_ALLOWED: &str = "agent_category_not_allowed";
    pub const AGENT_SENSITIVITY_NOT_ALLOWED: &str = "agent_sensitivity_not_allowed";
    pub const APPROVAL_TOKEN_INVALID: &str = "approval_token_invalid";
}

/// Inputs the gate consumes for one call.
pub struct GateInputs<'a> {
    pub identity: &'a VerifiedIdentity,
    pub envelope: &'a RequestEnvelope,
    pub capability: Option<&'a CapabilityDescriptor>,
    /// Unix seconds at gate entry. Caller-supplied so tests
    /// can drive time deterministically.
    pub now: i64,
}

/// Live store dependency. Wrapped in `Arc` so the dispatch
/// bridge can clone it cheaply per call.
pub type AgentStoreHandle = Arc<AgentStore>;

/// Run the gate. Pure-ish: storage is read via the store
/// handle; no chronicle / task side effects happen here — the
/// dispatch bridge runs those based on the returned decision.
pub fn evaluate(store: Option<&AgentStoreHandle>, inputs: GateInputs<'_>) -> GateDecision {
    let Some(store) = store else {
        // No agent store configured (e.g. tests that exercise
        // the bridge without a coordinator-side store). Fall
        // through.
        return allow("no_agent_store");
    };

    // 1. Token-bearing call: token check is the only path
    //    that admits an APPROVAL_REQUIRED-category call.
    if let Some(token) = inputs.envelope.approval_token.as_deref() {
        match store.get_approval_by_token(token) {
            Ok(Some(record)) => {
                return evaluate_token(&record, inputs.envelope.method.as_str(), inputs.now);
            }
            Ok(None) | Err(AgentStoreError::NotFound(_)) => {
                return GateDecision::Deny(GateDeny {
                    reason: format!("unknown approval_token: {token}"),
                    matched_rule: deny_reasons::APPROVAL_TOKEN_INVALID.into(),
                    agent_id: None,
                });
            }
            Err(e) => {
                // Storage hiccup — fail closed.
                return GateDecision::Deny(GateDeny {
                    reason: format!("approval token lookup: {e}"),
                    matched_rule: deny_reasons::APPROVAL_TOKEN_INVALID.into(),
                    agent_id: None,
                });
            }
        }
    }

    // 2. Categorical checks against the agent profile.
    let subject_id = inputs.identity.subject_id.to_string();
    let profile = match store.get_by_subject(&subject_id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            // No profile = backward-compat allow.
            return allow("no_agent_profile");
        }
        Err(e) => {
            return GateDecision::Deny(GateDeny {
                reason: format!("agent profile lookup: {e}"),
                matched_rule: "agent_profile_lookup_failed".into(),
                agent_id: None,
            });
        }
    };
    let view: AgentGateView = (&profile).into();
    evaluate_against_view(&view, inputs, store)
}

fn evaluate_token(record: &ApprovalRecord, request_method: &str, now: i64) -> GateDecision {
    if record.status != ApprovalStatus::Approved {
        return GateDecision::Deny(GateDeny {
            reason: format!("approval_token status={}", record.status.as_wire()),
            matched_rule: deny_reasons::APPROVAL_TOKEN_INVALID.into(),
            agent_id: Some(record.agent_id.clone()),
        });
    }
    if record.method != request_method {
        return GateDecision::Deny(GateDeny {
            reason: format!(
                "approval_token method mismatch: token=`{}` request=`{}`",
                record.method, request_method
            ),
            matched_rule: deny_reasons::APPROVAL_TOKEN_INVALID.into(),
            agent_id: Some(record.agent_id.clone()),
        });
    }
    if record.expires_at <= now {
        return GateDecision::Deny(GateDeny {
            reason: format!("approval_token expired at {}", record.expires_at),
            matched_rule: deny_reasons::APPROVAL_TOKEN_INVALID.into(),
            agent_id: Some(record.agent_id.clone()),
        });
    }
    GateDecision::Allow(GateAllow {
        matched_rule: "approval_token".into(),
        consumed_approval_id: Some(record.approval_id.clone()),
    })
}

fn evaluate_against_view(
    view: &AgentGateView,
    inputs: GateInputs<'_>,
    store: &AgentStoreHandle,
) -> GateDecision {
    // a) Status.
    match view.status.as_str() {
        "suspended" => {
            return deny(
                deny_reasons::AGENT_SUSPENDED,
                "agent status=suspended".into(),
                view,
            );
        }
        "disabled" => {
            return deny(
                deny_reasons::AGENT_DISABLED,
                "agent status=disabled".into(),
                view,
            );
        }
        "active" => {}
        other => {
            return deny(
                "agent_status_unknown",
                format!("unrecognised status: {other}"),
                view,
            );
        }
    }

    // b) Surface check.
    if !view.surface_allowlist.is_empty() {
        match inputs.envelope.surface.as_deref() {
            Some(s) if view.surface_allowlist.iter().any(|allowed| allowed == s) => {}
            other => {
                return deny(
                    deny_reasons::AGENT_SURFACE_DENIED,
                    format!(
                        "surface {} not in {:?}",
                        other.unwrap_or("<none>"),
                        view.surface_allowlist
                    ),
                    view,
                );
            }
        }
    }

    // c) Risk ceiling. Skipped when the call has no
    // CapabilityDescriptor (the gate doesn't synthesise a
    // descriptor for unknown methods).
    if let Some(cap) = inputs.capability {
        let risk_label = format!("{:?}", cap.risk_level).to_lowercase();
        if !risk_within_ceiling(&risk_label, &view.risk_ceiling) {
            return deny(
                deny_reasons::AGENT_RISK_CEILING_EXCEEDED,
                format!("risk={risk_label} > ceiling={}", view.risk_ceiling),
                view,
            );
        }

        // d) Deny list — categories.
        if cap
            .categories
            .iter()
            .any(|c| view.deny_categories.iter().any(|d| d == c))
        {
            return deny(
                deny_reasons::AGENT_CATEGORY_DENIED,
                format!(
                    "category in deny list: cap={:?} deny={:?}",
                    cap.categories, view.deny_categories
                ),
                view,
            );
        }
        // d) Deny list — sensitivity tags.
        if cap
            .sensitivity_tags
            .iter()
            .any(|t| view.deny_sensitivity_tags.iter().any(|d| d == t))
        {
            return deny(
                deny_reasons::AGENT_SENSITIVITY_DENIED,
                format!(
                    "sensitivity tag in deny list: cap={:?} deny={:?}",
                    cap.sensitivity_tags, view.deny_sensitivity_tags
                ),
                view,
            );
        }
        // e) Allow list.
        if !view.allow_categories.is_empty()
            && !cap
                .categories
                .iter()
                .any(|c| view.allow_categories.iter().any(|a| a == c))
        {
            return deny(
                deny_reasons::AGENT_CATEGORY_NOT_ALLOWED,
                format!(
                    "no overlap with allow_categories: cap={:?} allow={:?}",
                    cap.categories, view.allow_categories
                ),
                view,
            );
        }
        if !view.allow_sensitivity_tags.is_empty()
            && !cap.sensitivity_tags.is_empty()
            && !cap
                .sensitivity_tags
                .iter()
                .all(|t| view.allow_sensitivity_tags.iter().any(|a| a == t))
        {
            return deny(
                deny_reasons::AGENT_SENSITIVITY_NOT_ALLOWED,
                format!(
                    "sensitivity tag outside allow list: cap={:?} allow={:?}",
                    cap.sensitivity_tags, view.allow_sensitivity_tags
                ),
                view,
            );
        }
        // f) Approval-required check. Categories that need
        // approval first take the standing-approval fast path.
        let needs_approval = cap
            .categories
            .iter()
            .any(|c| view.approval_required_categories.iter().any(|r| r == c));
        if needs_approval {
            // Standing approval covers the *first* matching
            // approval-required category.
            let matched_category = cap
                .categories
                .iter()
                .find(|c| view.approval_required_categories.iter().any(|r| r == *c))
                .cloned()
                .unwrap_or_default();
            let standing = store
                .has_active_standing(&view.agent_id, &matched_category, inputs.now)
                .unwrap_or(false);
            if standing {
                return GateDecision::Allow(GateAllow {
                    matched_rule: format!("standing_approval:{matched_category}"),
                    consumed_approval_id: None,
                });
            }
            return GateDecision::RequireApproval(GateApprovalRequest {
                agent_id: view.agent_id.clone(),
                subject_id: view.subject_id.clone(),
                method: inputs.envelope.method.clone(),
                category: matched_category,
                reason: format!(
                    "agent {} attempted {} (category={})",
                    view.agent_id,
                    inputs.envelope.method,
                    cap.categories.first().cloned().unwrap_or_default()
                ),
                approver_groups: vec!["ops".into(), "admin".into()],
                approval_timeout_secs: view.approval_timeout_secs,
                task_id: inputs
                    .envelope
                    .task_id
                    .as_ref()
                    .filter(|s| !s.trim().is_empty())
                    .cloned(),
            });
        }
    }

    GateDecision::Allow(GateAllow {
        matched_rule: "agent_gate_pass".into(),
        consumed_approval_id: None,
    })
}

fn allow(matched_rule: &str) -> GateDecision {
    GateDecision::Allow(GateAllow {
        matched_rule: matched_rule.to_string(),
        consumed_approval_id: None,
    })
}

fn deny(matched_rule: &str, reason: String, view: &AgentGateView) -> GateDecision {
    GateDecision::Deny(GateDeny {
        reason,
        matched_rule: matched_rule.to_string(),
        agent_id: Some(view.agent_id.clone()),
    })
}

/// Risk ordering — `safe < low < medium < high < critical`.
/// `level <= ceiling` allowed. Unknown levels are conservative:
/// they only pass when ceiling is `critical`.
fn risk_within_ceiling(level: &str, ceiling: &str) -> bool {
    fn rank(s: &str) -> Option<i32> {
        match s {
            "safe" => Some(0),
            "low" => Some(1),
            "medium" => Some(2),
            "high" => Some(3),
            "critical" => Some(4),
            "unknown" => Some(4),
            _ => None,
        }
    }
    match (rank(level), rank(ceiling)) {
        (Some(l), Some(c)) => l <= c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::capability::RiskLevel;
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, Timestamp, TraceId};
    use serde_bytes::ByteBuf;

    fn store() -> AgentStoreHandle {
        Arc::new(AgentStore::in_memory().unwrap())
    }

    fn ident(subject_hex: &[u8]) -> VerifiedIdentity {
        VerifiedIdentity {
            subject_id: NodeId::from_pubkey(subject_hex),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(b"org"),
            groups: vec![],
            role: "".into(),
            clearance: "".into(),
            bundle_id: [0; 32],
        }
    }

    fn dummy_bundle() -> relix_core::bundle::Bundle {
        relix_core::bundle::Bundle {
            header: relix_core::bundle::BundleHeader {
                format_version: 1,
                alg: -8,
                kid: NodeId([0; 32]),
                bundle_type: relix_core::bundle::BundleType::Identity,
                issued_at: 0,
                not_before: 0,
                not_after: 9_999_999_999,
                bundle_serial: [0; 16],
            },
            payload: ByteBuf::new(),
            signature: [0; 64],
        }
    }

    fn env(method: &str, surface: Option<&str>) -> RequestEnvelope {
        RequestEnvelope {
            pv: 1,
            rid: RequestId([0u8; 16]),
            tid: TraceId::new(),
            method: method.into(),
            mv: 1,
            args: ByteBuf::new(),
            identity_bundle: dummy_bundle(),
            deadline: Timestamp::now().add_secs(30),
            surface: surface.map(|s| s.to_string()),
            approval_token: None,
            task_id: None,
            tenant_id: None,
        }
    }

    fn cap(categories: &[&str], tags: &[&str], risk: RiskLevel) -> CapabilityDescriptor {
        let mut c = CapabilityDescriptor::unary("tool.x");
        c.categories = categories.iter().map(|s| (*s).into()).collect();
        c.sensitivity_tags = tags.iter().map(|s| (*s).into()).collect();
        c.risk_level = risk;
        c
    }

    fn run(
        store: &AgentStoreHandle,
        identity: &VerifiedIdentity,
        envelope: &RequestEnvelope,
        cap: Option<&CapabilityDescriptor>,
    ) -> GateDecision {
        evaluate(
            Some(store),
            GateInputs {
                identity,
                envelope,
                capability: cap,
                now: 1_700_000_000,
            },
        )
    }

    // ── backward compat ──────────────────────────────────

    #[test]
    fn no_profile_admits_unchanged() {
        let s = store();
        let id = ident(b"unknown-subject");
        let e = env("tool.web_fetch", Some("api"));
        let cap = cap(&["fetch"], &["external:network"], RiskLevel::Low);
        let d = run(&s, &id, &e, Some(&cap));
        match d {
            GateDecision::Allow(a) => assert_eq!(a.matched_rule, "no_agent_profile"),
            other => panic!("expected Allow, got {other:?}"),
        }
    }

    #[test]
    fn store_handle_none_admits() {
        let id = ident(b"x");
        let e = env("m", None);
        let d = evaluate(
            None,
            GateInputs {
                identity: &id,
                envelope: &e,
                capability: None,
                now: 0,
            },
        );
        assert!(matches!(d, GateDecision::Allow(_)));
    }

    // ── status checks ────────────────────────────────────

    fn setup_with_profile(
        risk_ceiling: &str,
        status: &str,
        allow_cats: &[&str],
        deny_cats: &[&str],
        approval_required: &[&str],
    ) -> (AgentStoreHandle, VerifiedIdentity) {
        let s = store();
        let subject = NodeId::from_pubkey(b"agent-subject").to_string();
        let agent_id = s
            .create_agent(
                "Alice",
                "research",
                "Junior",
                "rd",
                "ops",
                "creator",
                &subject,
                risk_ceiling,
            )
            .unwrap();
        s.update_agent_field(&agent_id, "status", status).unwrap();
        if !allow_cats.is_empty() {
            s.update_agent_field(&agent_id, "allow_categories", &allow_cats.join(","))
                .unwrap();
        }
        if !deny_cats.is_empty() {
            s.update_agent_field(&agent_id, "deny_categories", &deny_cats.join(","))
                .unwrap();
        }
        if !approval_required.is_empty() {
            s.update_agent_field(
                &agent_id,
                "approval_required_categories",
                &approval_required.join(","),
            )
            .unwrap();
        } else {
            // Disable the default approval list for tests that
            // don't care about it.
            s.update_agent_field(&agent_id, "approval_required_categories", "")
                .unwrap();
        }
        let id = ident(b"agent-subject");
        (s, id)
    }

    #[test]
    fn suspended_agent_is_denied_with_agent_suspended() {
        let (s, id) = setup_with_profile("high", "suspended", &[], &[], &[]);
        let e = env("tool.x", None);
        let c = cap(&["fetch"], &[], RiskLevel::Low);
        match run(&s, &id, &e, Some(&c)) {
            GateDecision::Deny(d) => {
                assert_eq!(d.matched_rule, deny_reasons::AGENT_SUSPENDED);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn disabled_agent_is_denied_with_agent_disabled() {
        let (s, id) = setup_with_profile("high", "disabled", &[], &[], &[]);
        let e = env("tool.x", None);
        let c = cap(&["fetch"], &[], RiskLevel::Low);
        match run(&s, &id, &e, Some(&c)) {
            GateDecision::Deny(d) => {
                assert_eq!(d.matched_rule, deny_reasons::AGENT_DISABLED);
            }
            other => panic!("{other:?}"),
        }
    }

    // ── surface check ────────────────────────────────────

    #[test]
    fn surface_not_in_allowlist_is_denied() {
        let (s, id) = setup_with_profile("high", "active", &[], &[], &[]);
        let agent_id = s.list_agents(None).unwrap()[0].agent_id.clone();
        s.update_agent_field(&agent_id, "surface_allowlist", "scheduler,internal")
            .unwrap();
        let e = env("tool.x", Some("api"));
        let c = cap(&["fetch"], &[], RiskLevel::Low);
        match run(&s, &id, &e, Some(&c)) {
            GateDecision::Deny(d) => {
                assert_eq!(d.matched_rule, deny_reasons::AGENT_SURFACE_DENIED);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn empty_surface_allowlist_means_all_surfaces() {
        let (s, id) = setup_with_profile("high", "active", &[], &[], &[]);
        let e = env("tool.x", Some("api"));
        let c = cap(&["fetch"], &[], RiskLevel::Low);
        assert!(matches!(run(&s, &id, &e, Some(&c)), GateDecision::Allow(_)));
    }

    // ── risk ceiling ─────────────────────────────────────

    #[test]
    fn risk_above_ceiling_is_denied() {
        let (s, id) = setup_with_profile("medium", "active", &[], &[], &[]);
        let e = env("tool.x", None);
        let c = cap(&["fetch"], &[], RiskLevel::High);
        match run(&s, &id, &e, Some(&c)) {
            GateDecision::Deny(d) => {
                assert_eq!(d.matched_rule, deny_reasons::AGENT_RISK_CEILING_EXCEEDED);
            }
            other => panic!("{other:?}"),
        }
    }

    // ── deny / allow lists ──────────────────────────────

    #[test]
    fn category_in_deny_list_is_denied() {
        let (s, id) = setup_with_profile("high", "active", &[], &["payments"], &[]);
        let e = env("tool.x", None);
        let c = cap(&["payments"], &[], RiskLevel::Low);
        match run(&s, &id, &e, Some(&c)) {
            GateDecision::Deny(d) => {
                assert_eq!(d.matched_rule, deny_reasons::AGENT_CATEGORY_DENIED);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn category_not_in_allow_list_is_denied() {
        let (s, id) = setup_with_profile("high", "active", &["browser"], &[], &[]);
        let e = env("tool.x", None);
        let c = cap(&["fetch"], &[], RiskLevel::Low);
        match run(&s, &id, &e, Some(&c)) {
            GateDecision::Deny(d) => {
                assert_eq!(d.matched_rule, deny_reasons::AGENT_CATEGORY_NOT_ALLOWED);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn empty_allow_list_admits_any_category() {
        let (s, id) = setup_with_profile("high", "active", &[], &[], &[]);
        let e = env("tool.x", None);
        let c = cap(&["literally_anything"], &[], RiskLevel::Low);
        assert!(matches!(run(&s, &id, &e, Some(&c)), GateDecision::Allow(_)));
    }

    #[test]
    fn deny_sensitivity_tag_blocks_call() {
        let (s, id) = setup_with_profile("high", "active", &[], &[], &[]);
        let agent_id = s.list_agents(None).unwrap()[0].agent_id.clone();
        s.update_agent_field(&agent_id, "deny_sensitivity_tags", "credentials:read")
            .unwrap();
        let e = env("tool.x", None);
        let c = cap(&["read"], &["credentials:read"], RiskLevel::Low);
        match run(&s, &id, &e, Some(&c)) {
            GateDecision::Deny(d) => {
                assert_eq!(d.matched_rule, deny_reasons::AGENT_SENSITIVITY_DENIED);
            }
            other => panic!("{other:?}"),
        }
    }

    // ── approval-required ───────────────────────────────

    #[test]
    fn approval_required_returns_require_approval() {
        let (s, id) = setup_with_profile("high", "active", &[], &[], &["payments"]);
        let e = env("tool.x", None);
        let c = cap(&["payments"], &[], RiskLevel::Low);
        match run(&s, &id, &e, Some(&c)) {
            GateDecision::RequireApproval(req) => {
                assert_eq!(req.category, "payments");
                assert_eq!(req.method, "tool.x");
                // No task_id on the envelope → None on the
                // GateApprovalRequest.
                assert_eq!(req.task_id, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn require_approval_carries_envelope_task_id_through() {
        // When the caller threaded a task_id on the envelope,
        // the gate must surface it on the GateApprovalRequest so
        // the coordinator-side on_require_approval closure can
        // stamp it on the approval row + flip the task.
        let (s, id) = setup_with_profile("high", "active", &[], &[], &["payments"]);
        let mut e = env("tool.x", None);
        e.task_id = Some("task-42".into());
        let c = cap(&["payments"], &[], RiskLevel::Low);
        match run(&s, &id, &e, Some(&c)) {
            GateDecision::RequireApproval(req) => {
                assert_eq!(req.task_id.as_deref(), Some("task-42"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn require_approval_treats_empty_string_task_id_as_none() {
        // Defence in depth — older bridge code that stamps an
        // empty string on the envelope shouldn't end up writing
        // task_id = "" on the approval row.
        let (s, id) = setup_with_profile("high", "active", &[], &[], &["payments"]);
        let mut e = env("tool.x", None);
        e.task_id = Some("".into());
        let c = cap(&["payments"], &[], RiskLevel::Low);
        match run(&s, &id, &e, Some(&c)) {
            GateDecision::RequireApproval(req) => {
                assert_eq!(req.task_id, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn active_standing_approval_admits_without_approval_request() {
        let (s, id) = setup_with_profile("high", "active", &[], &[], &["payments"]);
        let agent_id = s.list_agents(None).unwrap()[0].agent_id.clone();
        s.create_standing(&agent_id, "payments", None, 9_999_999_999, "alice", "")
            .unwrap();
        let e = env("tool.x", None);
        let c = cap(&["payments"], &[], RiskLevel::Low);
        match run(&s, &id, &e, Some(&c)) {
            GateDecision::Allow(a) => {
                assert!(a.matched_rule.starts_with("standing_approval:"));
            }
            other => panic!("{other:?}"),
        }
    }

    // ── approval token ──────────────────────────────────

    #[test]
    fn unknown_approval_token_denies_with_token_invalid_matched_rule() {
        let (s, id) = setup_with_profile("high", "active", &[], &[], &[]);
        let mut e = env("tool.x", None);
        e.approval_token = Some("totally-fake".into());
        let c = cap(&["fetch"], &[], RiskLevel::Low);
        match run(&s, &id, &e, Some(&c)) {
            GateDecision::Deny(d) => {
                assert_eq!(d.matched_rule, deny_reasons::APPROVAL_TOKEN_INVALID);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn approved_token_for_matching_method_admits() {
        let s = store();
        let approval_id = s
            .create_approval(
                "agt-1",
                "subj-1",
                "tool.payments.charge",
                "payments",
                "",
                "",
                &[],
                None,
                9_999_999_999,
            )
            .unwrap();
        let token = s
            .decide_approval(&approval_id, ApprovalStatus::Approved, "alice", "")
            .unwrap()
            .unwrap();
        let id = ident(b"subj-1");
        let mut e = env("tool.payments.charge", None);
        e.approval_token = Some(token);
        let d = evaluate(
            Some(&s),
            GateInputs {
                identity: &id,
                envelope: &e,
                capability: None,
                now: 1_700_000_000,
            },
        );
        match d {
            GateDecision::Allow(a) => {
                assert_eq!(a.matched_rule, "approval_token");
                assert_eq!(
                    a.consumed_approval_id.as_deref(),
                    Some(approval_id.as_str())
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn approved_token_for_other_method_is_denied() {
        let s = store();
        let id = s
            .create_approval(
                "agt-1",
                "subj-1",
                "tool.payments.charge",
                "payments",
                "",
                "",
                &[],
                None,
                9_999_999_999,
            )
            .unwrap();
        let token = s
            .decide_approval(&id, ApprovalStatus::Approved, "alice", "")
            .unwrap()
            .unwrap();
        let i = ident(b"subj-1");
        let mut e = env("tool.web_fetch", None);
        e.approval_token = Some(token);
        let d = evaluate(
            Some(&s),
            GateInputs {
                identity: &i,
                envelope: &e,
                capability: None,
                now: 1_700_000_000,
            },
        );
        assert!(matches!(d, GateDecision::Deny(_)));
    }

    // ── policy-floor invariant ───────────────────────────

    #[test]
    fn policy_floor_holds_after_gate_allow() {
        // Locks the docstring claim: a `GateDecision::Allow`
        // doesn't *grant* anything — the dispatch bridge still
        // calls PolicyEngine::evaluate afterwards. This test is
        // a sentinel: any refactor that returns `Allow` from
        // the gate must not bypass the bridge's existing policy
        // step. The bridge tests cover the full chain; this one
        // documents the contract at the gate boundary.
        let (s, id) = setup_with_profile("high", "active", &[], &[], &[]);
        let e = env("tool.x", None);
        let c = cap(&["fetch"], &[], RiskLevel::Low);
        let d = run(&s, &id, &e, Some(&c));
        // Allow at this layer — the bridge calls policy next.
        match d {
            GateDecision::Allow(a) => {
                assert_eq!(a.matched_rule, "agent_gate_pass");
            }
            other => panic!("{other:?}"),
        }
    }
}
