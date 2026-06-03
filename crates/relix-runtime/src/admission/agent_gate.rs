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

use relix_core::capability::{CapabilityDescriptor, CostClass};
use relix_core::identity::VerifiedIdentity;

use crate::approval::{ApprovalKeySet, ApprovalToken, TokenError};
use crate::nodes::coordinator::agent::store::{
    AgentGateView, AgentStore, ApprovalStatus, StandingApprovalMatch,
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
    /// DEFERRED 2: snapshot of [`AgentGateView::authorized_approvers`]
    /// at gate time. The coordinator's `on_require_approval`
    /// closure stamps this on the freshly-minted approval row
    /// so `coord.approval.decide` can later check the
    /// operator's subject id against it. Empty ⇒ role-based
    /// fallback only.
    pub authorized_approvers: Vec<String>,
    /// GROUP 6: the request's VERIFIED tenant (from
    /// `RequestEnvelope::tenant_id`). The coordinator's
    /// `on_require_approval` closure stamps it on the
    /// `approval_requests` row so the table is tenant-isolated.
    pub tenant_id: String,
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
    /// SEC PART A: legacy catchall — kept for log compatibility
    /// when a non-token-specific failure happens. Specific
    /// token failure rules come from [`TokenError::matched_rule`].
    pub const APPROVAL_TOKEN_INVALID: &str = "approval_token_invalid";
    /// SEC PART 1 (default-deny): admission attempted while no
    /// agent store is wired. Previously admitted with
    /// `no_agent_store`; now fail-closed.
    pub const AGENT_STORE_NOT_CONFIGURED: &str = "agent_store_not_configured";
    /// SEC PART 1: caller's subject_id has no row in
    /// `agent_profiles`. Previously admitted with
    /// `no_agent_profile`; now fail-closed. Operators set up
    /// the agent via `relix agent create` (or seed
    /// `[[agents]]` in the controller TOML) before any
    /// admission attempt.
    pub const AGENT_NO_PROFILE: &str = "agent_no_profile";
}

/// SEC PART 1 (allow-all profile): matched_rule emitted when
/// `AgentProfile.profile == "allow-all"` bypasses every
/// categorical check. Audited as a distinct allow-rule so
/// operators can grep for bypasses in the chronicle.
pub const ALLOW_RULE_ALLOW_ALL_PROFILE: &str = "allow_all_profile";

/// SEC PART 1 (allow-all profile): the only operator-meaningful
/// `profile` label. Matches `AgentProfile.profile`.
pub const PROFILE_ALLOW_ALL: &str = "allow-all";

/// Inputs the gate consumes for one call.
pub struct GateInputs<'a> {
    pub identity: &'a VerifiedIdentity,
    pub envelope: &'a RequestEnvelope,
    pub capability: Option<&'a CapabilityDescriptor>,
    /// Unix seconds at gate entry. Caller-supplied so tests
    /// can drive time deterministically.
    pub now: i64,
    /// Unix milliseconds at gate entry. Used for token TTL
    /// checks (the token's `expires_at_ms` field is in ms;
    /// converting `now` to ms loses sub-second precision so
    /// the caller supplies both — admission paths read
    /// `unix_ms()` once and pass both).
    pub now_ms: i64,
    /// P1: Ed25519 verification key set for
    /// [`crate::approval::ApprovalToken`]. The gate looks up
    /// the wire token's `signing_key_fingerprint` here to find
    /// the public key it was signed under. Empty / unwired ⇒
    /// every token-bearing call fails with
    /// `approval_token_missing_key` (when the keyset is
    /// completely empty) or `approval_token_unknown_signer`
    /// (when the keyset has entries but none matches the
    /// wire fingerprint).
    pub keyset: &'a ApprovalKeySet,
    /// SEC PART 1: surface label derived from the transport
    /// layer connection metadata (peer alias from the libp2p
    /// `PeerId` of the calling peer). The gate consults THIS
    /// for `surface_allowlist` matching — `envelope.surface`
    /// is operator-asserted and therefore untrusted; it is
    /// ignored for admission decisions.
    ///
    /// `None` means the transport layer did not provide a
    /// surface (e.g. an internal test that didn't go through
    /// the controller event loop). The gate treats `None`
    /// exactly as it treated `envelope.surface = None` before
    /// PART 1 — denied when `surface_allowlist` is non-empty.
    pub caller_surface: Option<&'a str>,
}

/// Live store dependency. Wrapped in `Arc` so the dispatch
/// bridge can clone it cheaply per call.
pub type AgentStoreHandle = Arc<AgentStore>;

/// Run the gate. Pure-ish: storage is read via the store
/// handle; no chronicle / task side effects happen here — the
/// dispatch bridge runs those based on the returned decision.
pub fn evaluate(store: Option<&AgentStoreHandle>, inputs: GateInputs<'_>) -> GateDecision {
    // SEC PART 1: fail-closed when the store is not wired.
    // The pre-fix behaviour (silent allow) was a default-
    // permissive bypass — any deployment that booted without
    // an agent store admitted EVERY call. Operators wiring a
    // test stub get an explicit deny instead of latent
    // production-shaped bypass.
    let Some(store) = store else {
        return GateDecision::Deny(GateDeny {
            reason: "agent_gate: store not configured — all requests denied \
                     until agent store is wired"
                .into(),
            matched_rule: deny_reasons::AGENT_STORE_NOT_CONFIGURED.into(),
            agent_id: None,
        });
    };

    // 1. Token-bearing call: structured signed token is the
    //    only path that admits an APPROVAL_REQUIRED-category
    //    call. The verify_and_consume helper handles parse +
    //    HMAC verify (constant-time) + method scope + subject
    //    scope + expiry + atomic consume; failures map 1:1 to
    //    a specific [`TokenError`] variant.
    if let Some(token_wire) = inputs.envelope.approval_token.as_deref() {
        return evaluate_token(
            store,
            token_wire,
            inputs.envelope.method.as_str(),
            &inputs.identity.subject_id.to_string(),
            inputs.keyset,
            inputs.now_ms,
        );
    }

    // 2. Categorical checks against the agent profile.
    let subject_id = inputs.identity.subject_id.to_string();
    let profile = match store.get_by_subject(&subject_id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            // SEC PART 1: fail-closed when the agent has no
            // profile. Pre-fix: silent allow as "backward
            // compat." Operators register the agent (via
            // `relix agent create` or `[[agents]]` TOML)
            // BEFORE the agent is allowed to make any call.
            return GateDecision::Deny(GateDeny {
                reason: format!(
                    "agent_gate: no profile found for agent {subject_id} — \
                     register the agent before making requests"
                ),
                matched_rule: deny_reasons::AGENT_NO_PROFILE.into(),
                agent_id: None,
            });
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

/// SEC PART A: full structured-token admission path.
///
/// 1. Parse the wire token.
/// 2. Verify the HMAC-SHA256 signature with constant-time
///    compare. Bad signatures are rejected at this step;
///    nothing else is consulted (so a forged token does not
///    leak metadata about whether the approval row exists).
/// 3. Check the method binding — token issued for
///    `tool.web_read` is denied when used against
///    `tool.terminal`.
/// 4. Check the subject binding — caller's verified
///    subject_id MUST match the token's bound subject_id.
/// 5. Check expiry — `now_ms < token.expires_at_ms`.
/// 6. Fetch the approval row by id (NOT by token value) and
///    verify it is in `Approved` status. A row that has been
///    revoked / consumed / expired post-issue is denied here.
/// 7. Atomically claim the blocklist row via
///    [`AgentStore::try_consume_token_atomic`]. Two
///    concurrent requests with the same token: the loser
///    fails the UNIQUE-key insert and gets
///    `TokenError::AlreadyConsumed`.
pub fn evaluate_token(
    store: &AgentStoreHandle,
    token_wire: &str,
    request_method: &str,
    caller_subject_id: &str,
    keyset: &ApprovalKeySet,
    now_ms: i64,
) -> GateDecision {
    let tok = match ApprovalToken::parse(token_wire) {
        Ok(t) => t,
        Err(e) => return token_deny(&e, None),
    };
    if let Err(e) = tok.verify_signature(keyset) {
        return token_deny(&e, None);
    }
    if let Err(e) = tok.check_method(request_method) {
        return token_deny(&e, None);
    }
    if let Err(e) = tok.check_subject(caller_subject_id) {
        return token_deny(&e, None);
    }
    if let Err(e) = tok.check_not_expired(now_ms) {
        return token_deny(&e, None);
    }
    // Approval-row state check. The token is structurally
    // valid; the operator-side row may have been revoked or
    // already consumed via the legacy path.
    let record = match store.get_approval(&tok.approval_id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            // Token signature was valid AND we cannot find
            // the row. Treat as Store error so the audit log
            // surfaces a clear "approval not found" signal —
            // the signature already passed so this is not an
            // oracle for the caller.
            return token_deny(
                &TokenError::Store(format!("approval not found: {}", tok.approval_id)),
                None,
            );
        }
        Err(e) => {
            return token_deny(&TokenError::Store(e.to_string()), None);
        }
    };
    if record.status != ApprovalStatus::Approved {
        return token_deny(&TokenError::AlreadyConsumed, Some(record.agent_id.clone()));
    }
    let blocklist_key = tok.blocklist_key();
    let claimed = match store.try_consume_token_atomic(&blocklist_key, &tok.approval_id, now_ms) {
        Ok(c) => c,
        Err(e) => {
            return token_deny(
                &TokenError::Store(format!("consume: {e}")),
                Some(record.agent_id.clone()),
            );
        }
    };
    if !claimed {
        return token_deny(&TokenError::AlreadyConsumed, Some(record.agent_id.clone()));
    }
    GateDecision::Allow(GateAllow {
        matched_rule: "approval_token".into(),
        consumed_approval_id: Some(tok.approval_id),
    })
}

fn token_deny(err: &TokenError, agent_id: Option<String>) -> GateDecision {
    GateDecision::Deny(GateDeny {
        reason: err.to_string(),
        matched_rule: err.matched_rule().to_string(),
        agent_id,
    })
}

fn evaluate_against_view(
    view: &AgentGateView,
    inputs: GateInputs<'_>,
    store: &AgentStoreHandle,
) -> GateDecision {
    // SEC PART 1: `profile = "allow-all"` is the explicit
    // bypass. Runs BEFORE the status check so an operator can
    // hand a trusted internal agent unrestricted access
    // without ALSO toggling every categorical field. The
    // matched_rule is distinct so the chronicle / audit ring
    // makes the bypass visible.
    if view.profile.as_deref() == Some(PROFILE_ALLOW_ALL) {
        return GateDecision::Allow(GateAllow {
            matched_rule: ALLOW_RULE_ALLOW_ALL_PROFILE.to_string(),
            consumed_approval_id: None,
        });
    }
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
    //
    // SEC PART 1: source the surface from `inputs.caller_surface`
    // — the transport-layer-derived alias of the calling peer.
    // The previous read of `inputs.envelope.surface` trusted
    // an operator-asserted wire field; an off-policy caller
    // could claim any surface to bypass an allowlist designed
    // around scheduler-only / internal-only access. The
    // envelope's surface field is now ignored at admission.
    if !view.surface_allowlist.is_empty() {
        match inputs.caller_surface {
            Some(s) if view.surface_allowlist.iter().any(|allowed| allowed == s) => {}
            other => {
                return deny(
                    deny_reasons::AGENT_SURFACE_DENIED,
                    format!(
                        "transport surface {} not in {:?}",
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
            let standing_id = store
                .consume_active_standing_for(StandingApprovalMatch {
                    agent_id: &view.agent_id,
                    category: &matched_category,
                    method: &inputs.envelope.method,
                    task_id: inputs.envelope.task_id.as_deref().and_then(non_empty_str),
                    session_id: inputs
                        .envelope
                        .session_id
                        .as_deref()
                        .and_then(non_empty_str),
                    workspace_path: inputs
                        .envelope
                        .workspace_path
                        .as_deref()
                        .and_then(non_empty_str),
                    tenant_id: inputs.envelope.tenant_id.as_deref(),
                    estimated_cost_micros: standing_estimated_cost_micros(cap.cost_class),
                    now: inputs.now,
                })
                .unwrap_or(None);
            if let Some(standing_id) = standing_id {
                return GateDecision::Allow(GateAllow {
                    matched_rule: format!("standing_approval:{matched_category}:{standing_id}"),
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
                // DEFERRED 2: snapshot the operator-allow-list
                // from the agent profile so the coordinator's
                // `on_require_approval` closure can stamp it
                // on the freshly-minted approval_requests row.
                authorized_approvers: view.authorized_approvers.clone(),
                tenant_id: inputs
                    .envelope
                    .tenant_id
                    .clone()
                    .unwrap_or_else(|| "default".to_string()),
            });
        }
    }

    GateDecision::Allow(GateAllow {
        matched_rule: "agent_gate_pass".into(),
        consumed_approval_id: None,
    })
}

#[allow(dead_code)]
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

fn non_empty_str(s: &str) -> Option<&str> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn standing_estimated_cost_micros(cost_class: CostClass) -> i64 {
    match cost_class {
        CostClass::Cheap => 0,
        CostClass::Expensive => 1_000,
        CostClass::ExternalPaid => 10_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalSigner;
    use crate::nodes::coordinator::agent::store::StandingApprovalCreate;
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
            deadline: Timestamp::now()
                .add_secs(30)
                .expect("test clock not at i64::MAX"),
            issued_at_ms: 0,
            surface: surface.map(|s| s.to_string()),
            approval_token: None,
            task_id: None,
            session_id: None,
            workspace_path: None,
            tenant_id: None,
            session_token: None,
        }
    }

    fn cap(categories: &[&str], tags: &[&str], risk: RiskLevel) -> CapabilityDescriptor {
        let mut c = CapabilityDescriptor::unary("tool.x");
        c.categories = categories.iter().map(|s| (*s).into()).collect();
        c.sensitivity_tags = tags.iter().map(|s| (*s).into()).collect();
        c.risk_level = risk;
        c
    }

    /// P1: Ed25519 signer the test suite uses for structured-
    /// token minting + verification. Deterministic seed so
    /// tests can issue + replay against the same key.
    fn test_signer() -> ApprovalSigner {
        ApprovalSigner::from_seed([7u8; 32])
    }

    /// P1: verification keyset built from `test_signer` —
    /// the single-controller deployment shape.
    fn test_keyset() -> ApprovalKeySet {
        ApprovalKeySet::from_signer(&test_signer())
    }

    fn run(
        store: &AgentStoreHandle,
        identity: &VerifiedIdentity,
        envelope: &RequestEnvelope,
        cap: Option<&CapabilityDescriptor>,
    ) -> GateDecision {
        // Production paths derive caller_surface from the
        // libp2p PeerId. Tests that drive `evaluate` directly
        // need to mirror what the bridge would pass — the
        // envelope's own `surface` field is the test's
        // representation of the trusted transport-layer alias.
        run_with_surface(store, identity, envelope, cap, envelope.surface.as_deref())
    }

    fn run_with_surface(
        store: &AgentStoreHandle,
        identity: &VerifiedIdentity,
        envelope: &RequestEnvelope,
        cap: Option<&CapabilityDescriptor>,
        caller_surface: Option<&str>,
    ) -> GateDecision {
        let keyset = test_keyset();
        evaluate(
            Some(store),
            GateInputs {
                identity,
                envelope,
                capability: cap,
                now: 1_700_000_000,
                now_ms: 1_700_000_000_000,
                keyset: &keyset,
                caller_surface,
            },
        )
    }

    // ── SEC PART 1: default-deny posture ─────────────────

    #[test]
    fn no_profile_returns_security_denied() {
        // SEC PART 1: pre-fix behaviour was silent allow with
        // matched_rule "no_agent_profile". Fail-closed now.
        let s = store();
        let id = ident(b"unknown-subject");
        let e = env("tool.web_fetch", Some("api"));
        let cap = cap(&["fetch"], &["external:network"], RiskLevel::Low);
        let d = run(&s, &id, &e, Some(&cap));
        match d {
            GateDecision::Deny(deny) => {
                assert_eq!(deny.matched_rule, deny_reasons::AGENT_NO_PROFILE);
                assert!(
                    deny.reason.contains("no profile found"),
                    "reason: {}",
                    deny.reason
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn store_handle_none_returns_security_denied() {
        // SEC PART 1: pre-fix behaviour was silent allow with
        // matched_rule "no_agent_store". Fail-closed now.
        let id = ident(b"x");
        let e = env("m", None);
        let keyset = test_keyset();
        let d = evaluate(
            None,
            GateInputs {
                identity: &id,
                envelope: &e,
                capability: None,
                now: 0,
                now_ms: 0,
                keyset: &keyset,
                caller_surface: None,
            },
        );
        match d {
            GateDecision::Deny(deny) => {
                assert_eq!(deny.matched_rule, deny_reasons::AGENT_STORE_NOT_CONFIGURED);
                assert!(
                    deny.reason.contains("store not configured"),
                    "reason: {}",
                    deny.reason
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn allow_all_profile_bypasses_every_categorical_check() {
        // SEC PART 1: explicit profile = "allow-all" admits
        // through with matched_rule "allow_all_profile" even
        // when other fields would otherwise deny (e.g. risk
        // ceiling = safe + cap is critical).
        let (s, id) = setup_with_profile("safe", "active", &[], &[], &[]);
        let agent_id = s.list_agents(None).unwrap()[0].agent_id.clone();
        s.update_agent_field(&agent_id, "profile", "allow-all")
            .unwrap();
        let e = env("tool.payments.charge", None);
        // High-risk capability + categorical deny intent — the
        // bypass still admits because profile == allow-all.
        let c = cap(&["payments"], &[], RiskLevel::Critical);
        match run(&s, &id, &e, Some(&c)) {
            GateDecision::Allow(a) => {
                assert_eq!(a.matched_rule, ALLOW_RULE_ALLOW_ALL_PROFILE);
            }
            other => panic!("expected Allow(allow_all_profile), got {other:?}"),
        }
    }

    #[test]
    fn unknown_profile_label_is_rejected_by_update() {
        // The profile column is a strict allowlist — an
        // operator can't sneak in a new "allow-all-but-not"
        // label that the gate doesn't recognise.
        let (s, _id) = setup_with_profile("medium", "active", &[], &[], &[]);
        let agent_id = s.list_agents(None).unwrap()[0].agent_id.clone();
        let err = s
            .update_agent_field(&agent_id, "profile", "permissive")
            .expect_err("unknown profile must error");
        assert!(format!("{err:?}").contains("not recognised"), "got {err:?}");
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
                "default",
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

    #[test]
    fn envelope_surface_is_ignored_for_admission_decisions() {
        // SEC PART 1: the operator-asserted envelope.surface
        // must NOT influence the surface check. We set
        // surface_allowlist = ["scheduler"] but pass an
        // envelope claiming `scheduler` while the trusted
        // transport-layer surface is `api`. The gate must
        // consult the trusted surface only and deny.
        let (s, id) = setup_with_profile("high", "active", &[], &[], &[]);
        let agent_id = s.list_agents(None).unwrap()[0].agent_id.clone();
        s.update_agent_field(&agent_id, "surface_allowlist", "scheduler")
            .unwrap();
        let e = env("tool.x", Some("scheduler")); // forged
        let c = cap(&["fetch"], &[], RiskLevel::Low);
        // Trusted surface from the transport: "api". Forged
        // envelope claims "scheduler". Gate must DENY.
        let d = run_with_surface(&s, &id, &e, Some(&c), Some("api"));
        match d {
            GateDecision::Deny(deny) => {
                assert_eq!(deny.matched_rule, deny_reasons::AGENT_SURFACE_DENIED);
                assert!(
                    deny.reason.contains("transport surface api"),
                    "reason: {}",
                    deny.reason
                );
            }
            other => panic!("expected Deny via trusted surface, got {other:?}"),
        }
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
        s.create_standing(
            &agent_id,
            "payments",
            None,
            9_999_999_999,
            "alice",
            "",
            "default",
        )
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

    // ── SEC PART A — structured approval token ──────────

    #[test]
    fn task_scoped_standing_approval_does_not_admit_another_task() {
        let (s, id) = setup_with_profile("high", "active", &[], &[], &["payments"]);
        let agent_id = s.list_agents(None).unwrap()[0].agent_id.clone();
        s.create_scoped_standing(StandingApprovalCreate {
            agent_id: &agent_id,
            match_category: "payments",
            match_path_glob: None,
            scope_kind: Some("task"),
            task_id: Some("task-approved"),
            session_id: None,
            method_prefix: None,
            workspace_path_glob: None,
            expires_at: 9_999_999_999,
            granted_by: "alice",
            max_calls: None,
            max_cost_micros: None,
            note: "",
            tenant_id: "default",
        })
        .unwrap();
        let c = cap(&["payments"], &[], RiskLevel::Low);

        let mut approved = env("tool.payments.refund", None);
        approved.task_id = Some("task-approved".into());
        assert!(matches!(
            run(&s, &id, &approved, Some(&c)),
            GateDecision::Allow(_)
        ));

        let mut other = env("tool.payments.refund", None);
        other.task_id = Some("task-other".into());
        assert!(matches!(
            run(&s, &id, &other, Some(&c)),
            GateDecision::RequireApproval(_)
        ));
    }

    #[test]
    fn session_scoped_standing_approval_matches_envelope_session_id() {
        let (s, id) = setup_with_profile("high", "active", &[], &[], &["payments"]);
        let agent_id = s.list_agents(None).unwrap()[0].agent_id.clone();
        s.create_scoped_standing(StandingApprovalCreate {
            agent_id: &agent_id,
            match_category: "payments",
            match_path_glob: None,
            scope_kind: Some("session"),
            task_id: None,
            session_id: Some("sess-approved"),
            method_prefix: None,
            workspace_path_glob: None,
            expires_at: 9_999_999_999,
            granted_by: "alice",
            max_calls: None,
            max_cost_micros: None,
            note: "",
            tenant_id: "default",
        })
        .unwrap();
        let c = cap(&["payments"], &[], RiskLevel::Low);

        let mut approved = env("tool.payments.refund", None);
        approved.session_id = Some("sess-approved".into());
        assert!(matches!(
            run(&s, &id, &approved, Some(&c)),
            GateDecision::Allow(_)
        ));

        let mut other = env("tool.payments.refund", None);
        other.session_id = Some("sess-other".into());
        assert!(matches!(
            run(&s, &id, &other, Some(&c)),
            GateDecision::RequireApproval(_)
        ));
    }

    #[test]
    fn bounded_standing_approval_stops_admitting_after_max_calls() {
        let (s, id) = setup_with_profile("high", "active", &[], &[], &["payments"]);
        let agent_id = s.list_agents(None).unwrap()[0].agent_id.clone();
        s.create_scoped_standing(StandingApprovalCreate {
            agent_id: &agent_id,
            match_category: "payments",
            match_path_glob: None,
            scope_kind: Some("agent_category"),
            task_id: None,
            session_id: None,
            method_prefix: None,
            workspace_path_glob: None,
            expires_at: 9_999_999_999,
            granted_by: "alice",
            max_calls: Some(1),
            max_cost_micros: None,
            note: "one call only",
            tenant_id: "default",
        })
        .unwrap();
        let c = cap(&["payments"], &[], RiskLevel::Low);
        let e = env("tool.payments.refund", None);

        assert!(matches!(run(&s, &id, &e, Some(&c)), GateDecision::Allow(_)));
        assert!(matches!(
            run(&s, &id, &e, Some(&c)),
            GateDecision::RequireApproval(_)
        ));
    }

    #[test]
    fn cost_bounded_standing_approval_stops_admitting_after_budget() {
        let (s, id) = setup_with_profile("high", "active", &[], &[], &["payments"]);
        let agent_id = s.list_agents(None).unwrap()[0].agent_id.clone();
        s.create_scoped_standing(StandingApprovalCreate {
            agent_id: &agent_id,
            match_category: "payments",
            match_path_glob: None,
            scope_kind: Some("agent_category"),
            task_id: None,
            session_id: None,
            method_prefix: None,
            workspace_path_glob: None,
            expires_at: 9_999_999_999,
            granted_by: "alice",
            max_calls: None,
            max_cost_micros: Some(10_000),
            note: "one paid call only",
            tenant_id: "default",
        })
        .unwrap();
        let mut c = cap(&["payments"], &[], RiskLevel::Low);
        c.cost_class = CostClass::ExternalPaid;
        let e = env("tool.payments.refund", None);

        assert!(matches!(run(&s, &id, &e, Some(&c)), GateDecision::Allow(_)));
        assert!(matches!(
            run(&s, &id, &e, Some(&c)),
            GateDecision::RequireApproval(_)
        ));
    }

    /// Test helper: mint an approved approval + return its
    /// freshly-signed wire token + subject_id bytes used to
    /// derive the matching VerifiedIdentity.
    fn approve_and_mint_token(
        s: &AgentStoreHandle,
        method: &str,
        subject_seed: &[u8],
        ttl_ms: i64,
        issued_at_ms: i64,
    ) -> (String, String) {
        let subject_id_hex = relix_core::types::NodeId::from_pubkey(subject_seed).to_string();
        let approval_id = s
            .create_approval(
                "agt-1",
                &subject_id_hex,
                method,
                "cat",
                "",
                "",
                &[],
                None,
                9_999_999_999,
                &[],
                "default",
            )
            .unwrap();
        let meta = s
            .decide_approval(&approval_id, ApprovalStatus::Approved, "alice", "")
            .unwrap()
            .expect("approved -> metadata");
        let wire = ApprovalToken::issue(
            &meta.approval_id,
            &meta.method,
            &meta.subject_id,
            meta.task_id.as_deref().unwrap_or(""),
            issued_at_ms,
            ttl_ms,
            &test_signer(),
        )
        .unwrap();
        (wire, approval_id)
    }

    #[test]
    fn malformed_approval_token_is_denied_with_specific_matched_rule() {
        let (s, id) = setup_with_profile("high", "active", &[], &[], &[]);
        let mut e = env("tool.x", None);
        e.approval_token = Some("totally-fake".into());
        let c = cap(&["fetch"], &[], RiskLevel::Low);
        match run(&s, &id, &e, Some(&c)) {
            GateDecision::Deny(d) => {
                // MalformedEncoding folds into the "malformed"
                // matched_rule (the operator does not need to
                // distinguish base64 vs JSON failures at audit
                // time).
                assert_eq!(d.matched_rule, "approval_token_malformed");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn structured_token_for_matching_method_and_subject_admits() {
        let s = store();
        let (wire, approval_id) = approve_and_mint_token(
            &s,
            "tool.payments.charge",
            b"subj-1",
            60_000,
            1_700_000_000_000,
        );
        let id = ident(b"subj-1");
        let mut e = env("tool.payments.charge", None);
        e.approval_token = Some(wire);
        let keyset = test_keyset();
        let d = evaluate(
            Some(&s),
            GateInputs {
                identity: &id,
                envelope: &e,
                capability: None,
                now: 1_700_000_000,
                now_ms: 1_700_000_000_500,
                keyset: &keyset,
                caller_surface: None,
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
    fn structured_token_scope_mismatch_is_denied() {
        // Token issued for tool.web_read rejected when used
        // against tool.terminal. Surface the specific
        // matched_rule so operators can filter logs.
        let s = store();
        let (wire, _) =
            approve_and_mint_token(&s, "tool.web_read", b"subj-1", 60_000, 1_700_000_000_000);
        let id = ident(b"subj-1");
        let mut e = env("tool.terminal", None);
        e.approval_token = Some(wire);
        let keyset = test_keyset();
        let d = evaluate(
            Some(&s),
            GateInputs {
                identity: &id,
                envelope: &e,
                capability: None,
                now: 1_700_000_000,
                now_ms: 1_700_000_000_500,
                keyset: &keyset,
                caller_surface: None,
            },
        );
        match d {
            GateDecision::Deny(d) => {
                assert_eq!(d.matched_rule, "approval_token_scope_mismatch");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn structured_token_subject_mismatch_is_denied() {
        // Token issued to subj-A → caller subj-B is denied.
        let s = store();
        let (wire, _) = approve_and_mint_token(&s, "tool.x", b"subj-A", 60_000, 1_700_000_000_000);
        let id = ident(b"subj-B");
        let mut e = env("tool.x", None);
        e.approval_token = Some(wire);
        let keyset = test_keyset();
        let d = evaluate(
            Some(&s),
            GateInputs {
                identity: &id,
                envelope: &e,
                capability: None,
                now: 1_700_000_000,
                now_ms: 1_700_000_000_500,
                keyset: &keyset,
                caller_surface: None,
            },
        );
        match d {
            GateDecision::Deny(d) => {
                assert_eq!(d.matched_rule, "approval_token_subject_mismatch");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn structured_token_expired_is_denied() {
        let s = store();
        // Mint with issued_at_ms a long way back + small TTL.
        let (wire, _) = approve_and_mint_token(&s, "tool.x", b"subj-1", 1_000, 1_700_000_000_000);
        let id = ident(b"subj-1");
        let mut e = env("tool.x", None);
        e.approval_token = Some(wire);
        let keyset = test_keyset();
        // now_ms is well past expires_at_ms.
        let d = evaluate(
            Some(&s),
            GateInputs {
                identity: &id,
                envelope: &e,
                capability: None,
                now: 1_700_000_500,
                now_ms: 1_700_000_500_000,
                keyset: &keyset,
                caller_surface: None,
            },
        );
        match d {
            GateDecision::Deny(d) => {
                assert_eq!(d.matched_rule, "approval_token_expired");
            }
            other => panic!("{other:?}"),
        }
    }

    // DEFERRED A: end-to-end gate-level TTL boundary tests via
    // clock injection on `GateInputs::now_ms`. Each test issues
    // a token with `expires_at_ms = issued + 60_000` and then
    // drives the gate at three precise clock values:
    // expires - 1 (admit), expires (reject), expires + 1
    // (reject). No `tokio::time::sleep` — purely synthetic
    // time, runs in microseconds.

    /// Helper: issue a token at `issued_at_ms` with `ttl_ms`,
    /// evaluate the gate at `now_ms`, and return the verdict.
    /// Pulled out so the three boundary tests share one
    /// 12-line setup.
    fn evaluate_at_clock(
        s: &AgentStoreHandle,
        issued_at_ms: i64,
        ttl_ms: i64,
        now_ms: i64,
    ) -> GateDecision {
        let (wire, _) = approve_and_mint_token(s, "tool.x", b"subj-1", ttl_ms, issued_at_ms);
        let id = ident(b"subj-1");
        let mut e = env("tool.x", None);
        e.approval_token = Some(wire);
        let keyset = test_keyset();
        evaluate(
            Some(s),
            GateInputs {
                identity: &id,
                envelope: &e,
                capability: None,
                now: now_ms / 1_000,
                now_ms,
                keyset: &keyset,
                caller_surface: None,
            },
        )
    }

    #[test]
    fn gate_admits_token_one_ms_before_ttl_expires() {
        // expires_at_ms = 1_700_000_000_000 + 60_000 = 1_700_000_060_000
        // now_ms = expires - 1 = 1_700_000_059_999 → admit.
        let s = store();
        let d = evaluate_at_clock(&s, 1_700_000_000_000, 60_000, 1_700_000_059_999);
        assert!(matches!(d, GateDecision::Allow(_)), "got {d:?}");
    }

    #[test]
    fn gate_rejects_token_exactly_at_ttl_expiry() {
        // The exclusive-boundary check: now == expires_at_ms is
        // already expired. Locks the contract documented on
        // `TokenError::Expired`.
        let s = store();
        let d = evaluate_at_clock(&s, 1_700_000_000_000, 60_000, 1_700_000_060_000);
        match d {
            GateDecision::Deny(d) => assert_eq!(d.matched_rule, "approval_token_expired"),
            other => panic!("expected expired-at-boundary deny, got {other:?}"),
        }
    }

    #[test]
    fn gate_rejects_token_one_ms_after_ttl_expires() {
        // now_ms = expires + 1 = 1_700_000_060_001 → deny.
        let s = store();
        let d = evaluate_at_clock(&s, 1_700_000_000_000, 60_000, 1_700_000_060_001);
        match d {
            GateDecision::Deny(d) => assert_eq!(d.matched_rule, "approval_token_expired"),
            other => panic!("expected expired deny, got {other:?}"),
        }
    }

    #[test]
    fn structured_token_bad_signature_is_denied() {
        let s = store();
        let (wire, _) = approve_and_mint_token(&s, "tool.x", b"subj-1", 60_000, 1_700_000_000_000);
        let id = ident(b"subj-1");
        let mut e = env("tool.x", None);
        e.approval_token = Some(wire);
        // P1: a wrong-key verifier sees the wire fingerprint
        // belongs to a different signer → UnknownSigningKey.
        // Build a keyset that doesn't contain the test_signer
        // fingerprint at all.
        let other_signer = ApprovalSigner::from_seed([42u8; 32]);
        let wrong_keyset = ApprovalKeySet::from_signer(&other_signer);
        let d = evaluate(
            Some(&s),
            GateInputs {
                identity: &id,
                envelope: &e,
                capability: None,
                now: 1_700_000_000,
                now_ms: 1_700_000_000_500,
                keyset: &wrong_keyset,
                caller_surface: None,
            },
        );
        match d {
            GateDecision::Deny(d) => {
                assert_eq!(d.matched_rule, "approval_token_unknown_signer");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn consumed_token_cannot_be_replayed() {
        // Two requests with the same token: first wins, second
        // hits the SQLite blocklist row and is denied.
        let s = store();
        let (wire, _) = approve_and_mint_token(&s, "tool.x", b"subj-1", 60_000, 1_700_000_000_000);
        let id = ident(b"subj-1");
        let mut e = env("tool.x", None);
        e.approval_token = Some(wire);
        let keyset = test_keyset();
        let first = evaluate(
            Some(&s),
            GateInputs {
                identity: &id,
                envelope: &e,
                capability: None,
                now: 1_700_000_000,
                now_ms: 1_700_000_000_500,
                keyset: &keyset,
                caller_surface: None,
            },
        );
        assert!(matches!(first, GateDecision::Allow(_)));
        let replay = evaluate(
            Some(&s),
            GateInputs {
                identity: &id,
                envelope: &e,
                capability: None,
                now: 1_700_000_000,
                now_ms: 1_700_000_000_600,
                keyset: &keyset,
                caller_surface: None,
            },
        );
        match replay {
            GateDecision::Deny(d) => {
                assert_eq!(d.matched_rule, "approval_token_consumed");
            }
            other => panic!("expected consumed denial, got {other:?}"),
        }
    }

    #[test]
    fn token_compare_is_constant_time_under_tampered_signature_byte() {
        // Defence-in-depth assertion: the gate must reject ALL
        // tampered-signature tokens regardless of WHERE the
        // tampering happened. We can't measure timing in unit
        // tests reliably, so we verify the *behavioural*
        // contract: a single-byte signature flip in any
        // position yields BadSignature. The implementation
        // uses subtle::ConstantTimeEq so the underlying
        // primitive is constant-time by construction; this
        // test prevents a future refactor that drops the
        // contract.
        let s = store();
        let (wire, _) = approve_and_mint_token(&s, "tool.x", b"subj-1", 60_000, 1_700_000_000_000);
        let parsed = ApprovalToken::parse(&wire).unwrap();
        for i in 0..parsed.signature.len() {
            let mut tampered = parsed.clone();
            let mut sig_bytes: Vec<u8> = parsed.signature.bytes().collect();
            sig_bytes[i] ^= 0x01;
            tampered.signature = String::from_utf8(sig_bytes).unwrap();
            let tampered_wire = tampered.to_wire().unwrap();
            let id = ident(b"subj-1");
            let mut e = env("tool.x", None);
            e.approval_token = Some(tampered_wire);
            let keyset = test_keyset();
            let d = evaluate(
                Some(&s),
                GateInputs {
                    identity: &id,
                    envelope: &e,
                    capability: None,
                    now: 1_700_000_000,
                    now_ms: 1_700_000_000_500,
                    keyset: &keyset,
                    caller_surface: None,
                },
            );
            match d {
                GateDecision::Deny(d) => {
                    assert_eq!(
                        d.matched_rule, "approval_token_bad_signature",
                        "byte {i} flip must yield bad_signature"
                    );
                }
                other => panic!("byte {i} flip yielded {other:?}"),
            }
        }
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
