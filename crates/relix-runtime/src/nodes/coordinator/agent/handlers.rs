//! Capability handlers for the agent permission model.
//!
//! Wire formats land alongside each handler in the body
//! comment; the top-level table is documented in
//! `docs/agent-permissions.md`. Handlers live in a separate
//! file so the store module stays focused on storage.

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{HandlerOutcome, InvocationCtx};
use crate::nodes::coordinator::agent::store::{
    AgentStore, AgentStoreError, ApprovalStatus, default_approval_categories,
};

// ── agent.create ─────────────────────────────────────────

/// Wire arg: `name|role|title|department|team|created_by|subject_id|risk_ceiling`
pub fn handle_create(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("agent.create utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(8, '|').collect();
    if parts.len() != 8 {
        return invalid(
            "agent.create: expected `name|role|title|department|team|created_by|subject_id|risk_ceiling`".into(),
        );
    }
    match store.create_agent(
        parts[0], parts[1], parts[2], parts[3], parts[4], parts[5], parts[6], parts[7],
    ) {
        Ok(id) => HandlerOutcome::Ok(format!("{id}\n").into_bytes()),
        Err(AgentStoreError::BadInput(m)) => invalid(m),
        Err(e) => internal(format!("agent.create: {e}")),
    }
}

// ── agent.get ────────────────────────────────────────────

pub fn handle_get(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.get utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("agent.get: agent_id required".into());
    }
    match store.get_agent(id) {
        Ok(Some(p)) => {
            let body = format!(
                "agent_id={}|name={}|role={}|title={}|department={}|team={}|created_by={}|status={}|subject_id={}|risk_ceiling={}|approval_timeout_secs={}|created_at={}|updated_at={}|surface_allowlist={}|allow_categories={}|deny_categories={}|allow_sensitivity_tags={}|deny_sensitivity_tags={}|approval_required_categories={}\n",
                p.agent_id,
                sanitize(&p.name),
                sanitize(&p.role),
                sanitize(&p.title),
                sanitize(&p.department),
                sanitize(&p.team),
                sanitize(&p.created_by),
                p.status,
                p.subject_id,
                p.risk_ceiling,
                p.approval_timeout_secs,
                p.created_at,
                p.updated_at,
                csv(&p.surface_allowlist),
                csv(&p.allow_categories),
                csv(&p.deny_categories),
                csv(&p.allow_sensitivity_tags),
                csv(&p.deny_sensitivity_tags),
                csv(&p.approval_required_categories),
            );
            HandlerOutcome::Ok(body.into_bytes())
        }
        Ok(None) => invalid(format!("agent.get: not found: {id}")),
        Err(e) => internal(format!("agent.get: {e}")),
    }
}

// ── agent.list ───────────────────────────────────────────

pub fn handle_list(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let arg = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.list utf8: {e}")),
    };
    let filter = if arg.is_empty() { None } else { Some(arg) };
    match store.list_agents(filter) {
        Ok(rows) => {
            let mut out = String::new();
            for r in &rows {
                out.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\n",
                    r.agent_id,
                    sanitize(&r.name),
                    sanitize(&r.role),
                    r.status,
                    r.subject_id,
                ));
            }
            out.push_str(&format!("count={}\n", rows.len()));
            HandlerOutcome::Ok(out.into_bytes())
        }
        Err(e) => internal(format!("agent.list: {e}")),
    }
}

// ── agent.update ─────────────────────────────────────────

pub fn handle_update(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("agent.update utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(3, '|').collect();
    if parts.len() != 3 {
        return invalid("agent.update: expected `agent_id|field|value`".into());
    }
    match store.update_agent_field(parts[0], parts[1], parts[2]) {
        Ok(()) => HandlerOutcome::Ok(b"ok\n".to_vec()),
        Err(AgentStoreError::NotFound(_)) => {
            invalid(format!("agent.update: not found: {}", parts[0]))
        }
        Err(AgentStoreError::BadInput(m)) => invalid(m),
        Err(e) => internal(format!("agent.update: {e}")),
    }
}

// ── agent.delete (soft delete) ───────────────────────────

pub fn handle_delete(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.delete utf8: {e}")),
    };
    if id.is_empty() {
        return invalid("agent.delete: agent_id required".into());
    }
    match store.soft_delete_agent(id) {
        Ok(()) => HandlerOutcome::Ok(b"ok\n".to_vec()),
        Err(AgentStoreError::NotFound(_)) => invalid(format!("agent.delete: not found: {id}")),
        Err(e) => internal(format!("agent.delete: {e}")),
    }
}

// ── agent.effective_capabilities ─────────────────────────

/// Wire arg: `agent_id|peer_alias`. The handler reaches into the
/// dispatch bridge's manifest cache for `peer_alias`'s capability
/// descriptors, intersects them against the agent's categorical
/// permissions, and returns the set of permitted methods. The
/// manifest reader is wired via the closure in `register`.
pub fn handle_effective_capabilities<F>(
    store: &AgentStore,
    ctx: &InvocationCtx,
    fetch_peer_methods: F,
) -> HandlerOutcome
where
    F: Fn(&str) -> Vec<(String, Vec<String>, Vec<String>, String)>,
{
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("agent.effective_capabilities utf8: {e}")),
    };
    let (agent_id, peer_alias) = match s.split_once('|') {
        Some((a, p)) => (a.trim(), p.trim()),
        None => {
            return invalid("agent.effective_capabilities: expected `agent_id|peer_alias`".into());
        }
    };
    let agent = match store.get_agent(agent_id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return invalid(format!(
                "agent.effective_capabilities: not found: {agent_id}"
            ));
        }
        Err(e) => return internal(format!("agent.effective_capabilities: {e}")),
    };
    if agent.status != "active" {
        // Disabled / suspended agents have zero effective
        // capabilities — be explicit rather than returning
        // the empty intersection silently.
        return HandlerOutcome::Ok(
            format!("count=0\nreason=agent_{}\n", agent.status).into_bytes(),
        );
    }
    let caps = fetch_peer_methods(peer_alias);
    let mut allowed = Vec::new();
    for (method, categories, sensitivity_tags, risk_level) in caps {
        if !risk_within_ceiling(&risk_level, &agent.risk_ceiling) {
            continue;
        }
        if categories
            .iter()
            .any(|c| agent.deny_categories.iter().any(|d| d == c))
        {
            continue;
        }
        if sensitivity_tags
            .iter()
            .any(|t| agent.deny_sensitivity_tags.iter().any(|d| d == t))
        {
            continue;
        }
        if !agent.allow_categories.is_empty()
            && !categories
                .iter()
                .any(|c| agent.allow_categories.iter().any(|a| a == c))
        {
            continue;
        }
        allowed.push(method);
    }
    allowed.sort();
    allowed.dedup();
    let mut out = String::new();
    for m in &allowed {
        out.push_str(m);
        out.push('\n');
    }
    out.push_str(&format!("count={}\n", allowed.len()));
    HandlerOutcome::Ok(out.into_bytes())
}

/// `safe < low < medium < high < critical`. Returns true when
/// `level <= ceiling`. Unknown levels are treated as exceeding
/// every ceiling (conservative default).
pub fn risk_within_ceiling(level: &str, ceiling: &str) -> bool {
    fn rank(s: &str) -> Option<i32> {
        match s {
            "safe" => Some(0),
            "low" => Some(1),
            "medium" => Some(2),
            "high" => Some(3),
            "critical" => Some(4),
            _ => None,
        }
    }
    match (rank(level), rank(ceiling)) {
        (Some(l), Some(c)) => l <= c,
        _ => false,
    }
}

// ── coord.approval.pending ───────────────────────────────

pub fn handle_approval_pending(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let arg = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("coord.approval.pending utf8: {e}")),
    };
    let limit: usize = if arg.is_empty() {
        20
    } else {
        match arg.parse() {
            Ok(n) => n,
            Err(_) => return invalid(format!("coord.approval.pending: bad limit: {arg}")),
        }
    };
    match store.list_pending_approvals(limit) {
        Ok(rows) => {
            let mut out = String::new();
            for r in &rows {
                out.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\n",
                    r.approval_id,
                    r.agent_id,
                    r.method,
                    sanitize(&r.reason),
                    r.requested_at,
                ));
            }
            out.push_str(&format!("count={}\n", rows.len()));
            HandlerOutcome::Ok(out.into_bytes())
        }
        Err(e) => internal(format!("coord.approval.pending: {e}")),
    }
}

// ── coord.approval.decide ────────────────────────────────

pub type TaskResumeFn = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Wire arg: `approval_id|decision|decided_by|note`.
/// `decision` is `approved` or `rejected`.
/// On `approved`, returns `ok|<token>\n` and calls
/// `resume_task` to flip the waiting task back to `running`.
/// On `rejected`, returns `ok\n` and calls `fail_task`.
pub fn handle_approval_decide(
    store: &AgentStore,
    ctx: &InvocationCtx,
    resume_task: &TaskResumeFn,
    fail_task: &TaskResumeFn,
) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("coord.approval.decide utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(4, '|').collect();
    if parts.len() < 3 {
        return invalid(
            "coord.approval.decide: expected `approval_id|decision|decided_by|note?`".into(),
        );
    }
    let approval_id = parts[0].trim();
    let decision_raw = parts[1].trim();
    let decided_by = parts[2];
    let note = parts.get(3).copied().unwrap_or("");
    let decision = match decision_raw {
        "approved" => ApprovalStatus::Approved,
        "rejected" => ApprovalStatus::Rejected,
        other => return invalid(format!("coord.approval.decide: bad decision: {other}")),
    };
    // Capture the task_id BEFORE deciding so we can resume / fail
    // on the right row even if the decide call writes the
    // terminal state first.
    let record = match store.get_approval(approval_id) {
        Ok(Some(r)) => r,
        Ok(None) => return invalid(format!("coord.approval.decide: not found: {approval_id}")),
        Err(e) => return internal(format!("coord.approval.decide: {e}")),
    };
    let task_id = record.task_id.clone();
    let token = match store.decide_approval(approval_id, decision, decided_by, note) {
        Ok(t) => t,
        Err(AgentStoreError::NotFound(_)) => {
            return invalid(format!("coord.approval.decide: not found: {approval_id}"));
        }
        Err(AgentStoreError::BadInput(m)) => return invalid(m),
        Err(e) => return internal(format!("coord.approval.decide: {e}")),
    };
    if let Some(tid) = task_id.as_deref() {
        let r = match decision {
            ApprovalStatus::Approved => resume_task(tid),
            ApprovalStatus::Rejected => fail_task(tid),
            _ => Ok(()),
        };
        if let Err(e) = r {
            tracing::warn!(task_id = %tid, error = %e, "coord.approval.decide: task hop failed");
        }
    }
    let body = if let Some(t) = token {
        format!("ok|{t}\n")
    } else {
        "ok\n".to_string()
    };
    HandlerOutcome::Ok(body.into_bytes())
}

// ── standing approval handlers ──────────────────────────

/// `agent_id|category|expires_at|granted_by|note|path_glob?`
pub fn handle_standing_create(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("agent.standing_approval.create utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(6, '|').collect();
    if parts.len() < 5 {
        return invalid(
            "agent.standing_approval.create: expected `agent_id|category|expires_at|granted_by|note|path_glob?`"
                .into(),
        );
    }
    let agent_id = parts[0].trim();
    let category = parts[1].trim();
    let expires_at: i64 = match parts[2].trim().parse() {
        Ok(n) => n,
        Err(_) => {
            return invalid(format!(
                "agent.standing_approval.create: bad expires_at: {}",
                parts[2]
            ));
        }
    };
    let granted_by = parts[3].trim();
    let note = parts[4];
    let path_glob = parts.get(5).and_then(|p| {
        let t = p.trim();
        if t.is_empty() { None } else { Some(t) }
    });
    match store.create_standing(agent_id, category, path_glob, expires_at, granted_by, note) {
        Ok(id) => HandlerOutcome::Ok(format!("{id}\n").into_bytes()),
        Err(AgentStoreError::BadInput(m)) => invalid(m),
        Err(e) => internal(format!("agent.standing_approval.create: {e}")),
    }
}

pub fn handle_standing_list(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let agent_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.standing_approval.list utf8: {e}")),
    };
    if agent_id.is_empty() {
        return invalid("agent.standing_approval.list: agent_id required".into());
    }
    match store.list_standing(agent_id) {
        Ok(rows) => {
            let mut out = String::new();
            for r in &rows {
                out.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\n",
                    r.standing_id,
                    r.match_category,
                    r.match_path_glob.as_deref().unwrap_or(""),
                    r.expires_at,
                    r.granted_by,
                    sanitize(&r.note),
                ));
            }
            out.push_str(&format!("count={}\n", rows.len()));
            HandlerOutcome::Ok(out.into_bytes())
        }
        Err(e) => internal(format!("agent.standing_approval.list: {e}")),
    }
}

pub fn handle_standing_revoke(store: &AgentStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let standing_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("agent.standing_approval.revoke utf8: {e}")),
    };
    if standing_id.is_empty() {
        return invalid("agent.standing_approval.revoke: standing_id required".into());
    }
    match store.revoke_standing(standing_id) {
        Ok(()) => HandlerOutcome::Ok(b"ok\n".to_vec()),
        Err(AgentStoreError::NotFound(_)) => invalid(format!(
            "agent.standing_approval.revoke: not found: {standing_id}"
        )),
        Err(e) => internal(format!("agent.standing_approval.revoke: {e}")),
    }
}

// ── helpers ──────────────────────────────────────────────

fn invalid(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

fn internal(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause,
        retry_hint: 1,
        retry_after: None,
    })
}

fn sanitize(s: &str) -> String {
    s.replace('|', " ").replace(['\n', '\r', '\t'], " ")
}

fn csv(v: &[String]) -> String {
    v.join(",")
}

/// Re-export so the executor module and tests can reach for
/// the canonical default category list without re-importing
/// from the store module.
pub fn default_approval_required_categories() -> Vec<String> {
    default_approval_categories()
}

#[cfg(test)]
pub(crate) fn fake_ctx(args: &[u8]) -> InvocationCtx {
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};
    InvocationCtx {
        caller: VerifiedIdentity {
            subject_id: NodeId::from_pubkey(b"caller"),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(b"org"),
            groups: vec![],
            role: "".into(),
            clearance: "".into(),
            bundle_id: [0; 32],
        },
        trace_id: TraceId::new(),
        request_id: RequestId::new(),
        args: args.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> AgentStore {
        AgentStore::in_memory().unwrap()
    }

    fn ok_body(o: HandlerOutcome) -> String {
        match o {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok: {} {}", e.kind, e.cause),
        }
    }
    fn err_kind(o: HandlerOutcome) -> u32 {
        match o {
            HandlerOutcome::Ok(_) => panic!("expected Err"),
            HandlerOutcome::Err(e) => e.kind,
        }
    }

    #[test]
    fn create_handler_returns_agent_id() {
        let s = store();
        let out = handle_create(
            &s,
            &fake_ctx(b"Research|research|Junior|research|ops|alice|subj-1|medium"),
        );
        let id = ok_body(out).trim().to_string();
        assert!(id.starts_with("agt_research_"));
        let p = s.get_agent(&id).unwrap().unwrap();
        assert_eq!(p.name, "Research");
    }

    #[test]
    fn create_handler_rejects_wrong_pipe_count() {
        let s = store();
        let out = handle_create(&s, &fake_ctx(b"too|few|fields"));
        assert_eq!(err_kind(out), error_kinds::INVALID_ARGS);
    }

    #[test]
    fn get_handler_returns_all_fields() {
        let s = store();
        let id = s
            .create_agent(
                "Research", "research", "Junior", "rd", "ops", "alice", "subj-1", "medium",
            )
            .unwrap();
        let body = ok_body(handle_get(&s, &fake_ctx(id.as_bytes())));
        for needle in [
            "agent_id=",
            "name=Research",
            "role=research",
            "status=active",
            "risk_ceiling=medium",
            "subject_id=subj-1",
            "approval_required_categories=",
        ] {
            assert!(body.contains(needle), "missing {needle:?}: {body}");
        }
    }

    #[test]
    fn list_handler_filters_by_subject() {
        let s = store();
        s.create_agent("a", "r", "t", "d", "t", "c", "subj-1", "low")
            .unwrap();
        s.create_agent("b", "r", "t", "d", "t", "c", "subj-2", "low")
            .unwrap();
        let body = ok_body(handle_list(&s, &fake_ctx(b"subj-1")));
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1], "count=1");
    }

    #[test]
    fn update_handler_toggles_status() {
        let s = store();
        let id = s
            .create_agent("a", "r", "t", "d", "t", "c", "subj", "medium")
            .unwrap();
        let arg = format!("{id}|status|suspended");
        let out = handle_update(&s, &fake_ctx(arg.as_bytes()));
        assert_eq!(ok_body(out), "ok\n");
        assert_eq!(s.get_agent(&id).unwrap().unwrap().status, "suspended");
    }

    #[test]
    fn delete_handler_disables_the_row() {
        let s = store();
        let id = s
            .create_agent("a", "r", "t", "d", "t", "c", "subj", "medium")
            .unwrap();
        let out = handle_delete(&s, &fake_ctx(id.as_bytes()));
        assert_eq!(ok_body(out), "ok\n");
        assert_eq!(s.get_agent(&id).unwrap().unwrap().status, "disabled");
    }

    #[test]
    fn effective_capabilities_intersects_with_allow_categories() {
        let s = store();
        let id = s
            .create_agent("a", "r", "t", "d", "t", "c", "subj", "medium")
            .unwrap();
        s.update_agent_field(&id, "allow_categories", "browser, fetch")
            .unwrap();
        let arg = format!("{id}|ai");
        let out = handle_effective_capabilities(&s, &fake_ctx(arg.as_bytes()), |_| {
            vec![
                (
                    "tool.browser.click".into(),
                    vec!["browser".into()],
                    vec![],
                    "medium".into(),
                ),
                (
                    "tool.web_fetch".into(),
                    vec!["fetch".into()],
                    vec![],
                    "low".into(),
                ),
                (
                    "payments.charge".into(),
                    vec!["payments".into()],
                    vec![],
                    "high".into(),
                ),
            ]
        });
        let body = ok_body(out);
        assert!(body.contains("tool.browser.click"));
        assert!(body.contains("tool.web_fetch"));
        assert!(!body.contains("payments.charge"));
        assert!(body.contains("count=2"));
    }

    #[test]
    fn effective_capabilities_returns_zero_for_disabled_agent() {
        let s = store();
        let id = s
            .create_agent("a", "r", "t", "d", "t", "c", "subj", "medium")
            .unwrap();
        s.soft_delete_agent(&id).unwrap();
        let arg = format!("{id}|ai");
        let out = handle_effective_capabilities(&s, &fake_ctx(arg.as_bytes()), |_| Vec::new());
        let body = ok_body(out);
        assert!(body.contains("count=0"));
        assert!(body.contains("reason=agent_disabled"));
    }

    #[test]
    fn risk_within_ceiling_table() {
        assert!(risk_within_ceiling("low", "medium"));
        assert!(risk_within_ceiling("medium", "medium"));
        assert!(!risk_within_ceiling("high", "medium"));
        assert!(risk_within_ceiling("critical", "critical"));
        assert!(!risk_within_ceiling("garbage", "high"));
    }

    #[test]
    fn approval_pending_returns_correct_row_count() {
        let s = store();
        s.create_approval("a", "s", "m", "c", "", "r1", &[], None, 9999999999)
            .unwrap();
        s.create_approval("a", "s", "m", "c", "", "r2", &[], None, 9999999999)
            .unwrap();
        let body = ok_body(handle_approval_pending(&s, &fake_ctx(b"")));
        assert!(body.contains("count=2"));
    }

    #[test]
    fn approval_decide_approves_and_mints_token() {
        let s = store();
        let id = s
            .create_approval("a", "s", "m", "c", "", "", &[], None, 9999999999)
            .unwrap();
        let resume: TaskResumeFn = Arc::new(|_| Ok(()));
        let fail: TaskResumeFn = Arc::new(|_| Ok(()));
        let arg = format!("{id}|approved|alice|ok");
        let body = ok_body(handle_approval_decide(
            &s,
            &fake_ctx(arg.as_bytes()),
            &resume,
            &fail,
        ));
        assert!(body.starts_with("ok|"));
        let token = body.trim_start_matches("ok|").trim();
        assert_eq!(token.len(), 32);
    }

    #[test]
    fn approval_decide_rejects_returns_ok_without_token() {
        let s = store();
        let id = s
            .create_approval("a", "s", "m", "c", "", "", &[], None, 9999999999)
            .unwrap();
        let resume: TaskResumeFn = Arc::new(|_| Ok(()));
        let fail: TaskResumeFn = Arc::new(|_| Ok(()));
        let arg = format!("{id}|rejected|alice|nope");
        let body = ok_body(handle_approval_decide(
            &s,
            &fake_ctx(arg.as_bytes()),
            &resume,
            &fail,
        ));
        assert_eq!(body, "ok\n");
    }

    #[test]
    fn standing_create_then_list_then_revoke_round_trips() {
        let s = store();
        let arg = "agt-1|fs|9999999999|alice|monthly window";
        let id = ok_body(handle_standing_create(&s, &fake_ctx(arg.as_bytes())))
            .trim()
            .to_string();
        assert!(id.starts_with("std_"));
        let body = ok_body(handle_standing_list(&s, &fake_ctx(b"agt-1")));
        assert!(body.contains("count=1"));
        let body = ok_body(handle_standing_revoke(&s, &fake_ctx(id.as_bytes())));
        assert_eq!(body, "ok\n");
    }

    #[test]
    fn default_approval_required_categories_matches_spec() {
        let v = default_approval_required_categories();
        assert!(v.contains(&"payments".to_string()));
        assert!(v.contains(&"production_deploy".to_string()));
        assert!(v.contains(&"credentials:read".to_string()));
        assert!(v.contains(&"email:send".to_string()));
        assert!(v.contains(&"external_api:write".to_string()));
        assert!(v.contains(&"browser.form_submit".to_string()));
    }
}
