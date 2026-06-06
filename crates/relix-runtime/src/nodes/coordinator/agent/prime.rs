//! **Prime Assistant** — the governed "describe what you want → plan"
//! surface (company-model: the Prime proposes; the Founder approves).
//!
//! [`generate_proposal`] is a PURE, deterministic function: it never mutates
//! anything and never calls an LLM. No language model is wired into a
//! coordinator capability today (the AI node is reachable only as a separate
//! mesh peer, not synchronously from a handler), so the plan is rule-based and
//! the response says so honestly via `ai_status` — it is never silently faked
//! as model output. The function interprets the operator's request into a
//! structured plan: the interpreted intent, a proposed Mandate, the crew roles
//! it needs, suggested hires for any MISSING role, a Brief breakdown with
//! dependency edges, the risks/blockers, and the next actions the operator can
//! approve.
//!
//! `prime.propose` only WRITES the proposal record — it creates no Mandate, no
//! Brief, no Operative. `prime.approve` is the ONLY path that materializes the
//! plan, and even then the risky steps stay behind their existing governance
//! gates: a suggested hire becomes a `pending` Operative that still needs a
//! Clearance (never a fake active agent), and nothing runs an adapter, applies
//! a workspace, or touches budget automatically.

use serde::{Deserialize, Serialize};

/// Honest status string for the (currently rule-based) planning path.
pub const AI_STATUS_DETERMINISTIC: &str = "deterministic — no LLM is wired into \
     the coordinator, so this plan is rule-based; an LLM would refine the intent \
     interpretation and Brief breakdown. (Not silently presented as model output.)";

/// One crew role the plan needs, and whether an eligible active Operative
/// already fills it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrewSlot {
    pub role: String,
    pub have: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
}

/// A suggested hire for a role no active Operative fills. Approving the
/// proposal files this as a `pending` hire request (needs Clearance) — never a
/// fake active agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HireSuggestion {
    pub role: String,
    pub title: String,
    pub reason: String,
}

/// One proposed Brief in the breakdown. `key` is a stable identifier used as
/// the source marker on approval (idempotency) and to express dependencies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedBrief {
    pub key: String,
    pub title: String,
    pub role: String,
    /// Keys of other proposed Briefs this one is blocked on.
    pub depends_on: Vec<String>,
}

/// The full structured plan returned by `prime.propose`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrimeProposal {
    /// `build` / `fix` / `research` / `generic`.
    pub intent: String,
    pub summary: String,
    pub mandate_title: String,
    pub mandate_brief: String,
    pub roles: Vec<String>,
    pub crew: Vec<CrewSlot>,
    pub hires: Vec<HireSuggestion>,
    pub briefs: Vec<ProposedBrief>,
    pub risks: Vec<String>,
    pub next_actions: Vec<String>,
    /// Whether a language model shaped this proposal. Always `false` today.
    pub ai_used: bool,
    pub ai_status: String,
}

/// One existing Operative, for crew-matching.
#[derive(Debug, Clone)]
pub struct CrewMember {
    pub agent_id: String,
    pub name: String,
    pub role: String,
    pub status: String,
}

/// Collapse a free-form role string into a canonical role family. An unknown
/// role maps to `engineer` (the safe default work role).
pub fn canon_role(role: &str) -> &'static str {
    match role.trim().to_ascii_lowercase().as_str() {
        "engineer" | "engineering" | "swe" | "developer" | "dev" | "backend" | "frontend"
        | "fullstack" => "engineer",
        "designer" | "design" | "ux" | "ui" => "designer",
        "researcher" | "research" | "analyst" | "analysis" => "researcher",
        "writer" | "writing" | "content" | "copywriter" | "docs" => "writer",
        "qa" | "test" | "tester" | "quality" => "qa",
        "ops" | "devops" | "sre" | "operations" => "ops",
        // Note: `planner`/`prime` are leadership, not a work track.
        _ => "engineer",
    }
}

/// Human title for a canonical role.
fn role_title(role: &str) -> &'static str {
    match role {
        "engineer" => "Engineer",
        "designer" => "Designer",
        "researcher" => "Researcher",
        "writer" => "Writer",
        "qa" => "QA",
        "ops" => "Ops",
        _ => "Operative",
    }
}

/// Classify the request into an intent + the canonical work roles it needs.
fn classify(message: &str) -> (&'static str, Vec<&'static str>) {
    let m = message.to_ascii_lowercase();
    let has = |kw: &[&str]| kw.iter().any(|k| m.contains(k));

    let intent = if has(&["fix", "bug", "debug", "broken", "error", "crash", "regression"]) {
        "fix"
    } else if has(&[
        "research", "investigate", "explore", "analyze", "analyse", "compare", "evaluate", "spike",
    ]) {
        "research"
    } else if has(&[
        "build", "create", "make", "ship", "implement", "develop", "launch",
    ]) {
        "build"
    } else {
        "generic"
    };

    let mut roles: Vec<&'static str> = Vec::new();
    let push = |roles: &mut Vec<&'static str>, r: &'static str| {
        if !roles.contains(&r) {
            roles.push(r);
        }
    };
    if has(&[
        "dashboard", "web", "website", "site", "app", "frontend", "ui", "page", "interface",
        "screen",
    ]) {
        push(&mut roles, "engineer");
        push(&mut roles, "designer");
    }
    if has(&[
        "api", "backend", "server", "database", " db", "service", "endpoint", "auth", "login",
    ]) {
        push(&mut roles, "engineer");
    }
    if has(&["design", "mockup", "figma", "wireframe", "brand", "logo"]) {
        push(&mut roles, "designer");
    }
    if has(&[
        "research", "investigate", "analyze", "analyse", "compare", "evaluate", "spike",
    ]) {
        push(&mut roles, "researcher");
    }
    if has(&["test", "qa", "quality", "coverage"]) {
        push(&mut roles, "qa");
    }
    if has(&["write", "docs", "documentation", "blog", "content", "copy"]) {
        push(&mut roles, "writer");
    }
    if has(&["deploy", "infra", "ops", " ci", "pipeline", "release"]) {
        push(&mut roles, "ops");
    }
    if roles.is_empty() {
        roles.push("engineer");
    }
    (intent, roles)
}

/// Truncate to `max` chars on a word boundary, dropping trailing punctuation.
fn bound_title(s: &str, max: usize) -> String {
    let s = s.trim().trim_end_matches(['.', '!', '?', ',']);
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    match truncated.rfind(' ') {
        Some(i) if i > 20 => truncated[..i].trim_end().to_string(),
        _ => truncated,
    }
}

/// Derive a Mandate title from the request: strip a leading imperative phrase,
/// capitalize, and bound the length.
fn derive_title(message: &str) -> String {
    let mut t = message.trim().to_string();
    let lower = t.to_ascii_lowercase();
    for prefix in [
        "build me an ", "build me a ", "build me ", "build an ", "build a ", "build ",
        "create me an ", "create me a ", "create an ", "create a ", "create ", "make me an ",
        "make me a ", "make an ", "make a ", "make ", "i want to ", "i want an ", "i want a ",
        "i need an ", "i need a ", "i need to ", "please ", "can you ", "could you ", "help me ",
        "let's ", "lets ", "implement ", "ship ", "develop ", "set up ", "setup ",
    ] {
        if lower.starts_with(prefix) {
            // Prefixes are ASCII, so the byte offset is a valid char boundary.
            t = t[prefix.len()..].trim().to_string();
            break;
        }
    }
    if t.is_empty() {
        t = message.trim().to_string();
    }
    let mut chars = t.chars();
    let title = match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => t.clone(),
    };
    bound_title(&title, 80)
}

/// Build the intent-shaped Brief breakdown (company-model §12.5A). The
/// *shape* depends on the intent, and each Brief title carries the extracted
/// `subject` so the plan names WHAT is being done, not just a role:
///
/// - `fix` → a sequential *reproduce → fix → verify* chain (verify is QA
///   when a QA role is inferred, else the primary role);
/// - `research` → a sequential *investigate → synthesize* chain;
/// - `build` / `generic` → one role-track per inferred role + an
///   *integrate & ship* Brief that depends on every track (only when there
///   is more than one track).
///
/// PURE — the seam where a future model can author a richer breakdown while
/// reusing the same `ProposedBrief` contract and governed execution path.
fn build_briefs(intent: &str, roles: &[&'static str], subject: &str) -> Vec<ProposedBrief> {
    let primary = roles.first().copied().unwrap_or("engineer");
    match intent {
        "fix" => {
            // The fix lands on the primary work role; verification prefers a
            // QA Operative when the request implied one.
            let verifier = if roles.contains(&"qa") { "qa" } else { primary };
            vec![
                ProposedBrief {
                    key: "reproduce".into(),
                    title: format!("Reproduce: {subject}"),
                    role: primary.into(),
                    depends_on: Vec::new(),
                },
                ProposedBrief {
                    key: "fix".into(),
                    title: format!("Fix: {subject}"),
                    role: primary.into(),
                    depends_on: vec!["reproduce".into()],
                },
                ProposedBrief {
                    key: "verify".into(),
                    title: format!("Verify the fix: {subject}"),
                    role: verifier.into(),
                    depends_on: vec!["fix".into()],
                },
            ]
        }
        "research" => {
            let investigator = if roles.contains(&"researcher") {
                "researcher"
            } else {
                primary
            };
            // Write-up prefers a writer when one was inferred, else the
            // investigator carries it through.
            let writer = if roles.contains(&"writer") {
                "writer"
            } else {
                investigator
            };
            vec![
                ProposedBrief {
                    key: "investigate".into(),
                    title: format!("Investigate: {subject}"),
                    role: investigator.into(),
                    depends_on: Vec::new(),
                },
                ProposedBrief {
                    key: "synthesize".into(),
                    title: format!("Synthesize findings: {subject}"),
                    role: writer.into(),
                    depends_on: vec!["investigate".into()],
                },
            ]
        }
        // build / generic: role tracks + an integration Brief.
        _ => {
            let mut briefs = Vec::new();
            let mut track_keys = Vec::new();
            for r in roles {
                let canon = *r;
                let key = format!("track:{canon}");
                track_keys.push(key.clone());
                briefs.push(ProposedBrief {
                    key,
                    title: format!("{} track: {subject}", role_title(canon)),
                    role: canon.to_string(),
                    depends_on: Vec::new(),
                });
            }
            if track_keys.len() > 1 {
                briefs.push(ProposedBrief {
                    key: "integrate".into(),
                    title: format!("Integrate + ship: {subject}"),
                    role: "engineer".into(),
                    depends_on: track_keys,
                });
            }
            briefs
        }
    }
}

/// Build a deterministic Prime proposal from the (already secret-redacted)
/// request and the current Crew. PURE — mutates nothing.
pub fn generate_proposal(message: &str, crew: &[CrewMember]) -> PrimeProposal {
    let msg = message.trim();
    let (intent, role_refs) = classify(msg);
    let subject = derive_title(msg);
    let summary = match intent {
        "build" => format!("Build: {subject}"),
        "fix" => format!("Fix: {subject}"),
        "research" => format!("Research: {subject}"),
        _ => format!("Work: {subject}"),
    };

    // Intent-shaped breakdown FIRST, so crew/hires reflect the roles the plan
    // actually uses — every suggested hire maps to a Brief, and vice versa.
    let briefs = build_briefs(intent, &role_refs, &subject);
    let mut used_roles: Vec<&'static str> = Vec::new();
    for b in &briefs {
        let canon = canon_role(&b.role);
        if !used_roles.contains(&canon) {
            used_roles.push(canon);
        }
    }
    let roles: Vec<String> = used_roles.iter().map(|r| r.to_string()).collect();

    // Crew match: each used role wants an ACTIVE Operative in a matching
    // family; a missing role becomes a `pending` hire suggestion (never a
    // fake active agent).
    let mut crew_slots = Vec::new();
    let mut hires = Vec::new();
    for r in &used_roles {
        let canon = *r;
        let found = crew
            .iter()
            .find(|c| c.status == "active" && canon_role(&c.role) == canon);
        match found {
            Some(c) => crew_slots.push(CrewSlot {
                role: canon.to_string(),
                have: true,
                agent_id: Some(c.agent_id.clone()),
                agent_name: Some(c.name.clone()),
            }),
            None => {
                crew_slots.push(CrewSlot {
                    role: canon.to_string(),
                    have: false,
                    agent_id: None,
                    agent_name: None,
                });
                hires.push(HireSuggestion {
                    role: canon.to_string(),
                    title: role_title(canon).to_string(),
                    reason: format!("no active {} in the Guild", role_title(canon)),
                });
            }
        }
    }

    // Risks / blockers.
    let mut risks = Vec::new();
    let active_count = crew.iter().filter(|c| c.status == "active").count();
    if active_count == 0 {
        risks.push(
            "No active Operatives yet — hires must be approved (Clearance) before any Brief can be assigned or run."
                .to_string(),
        );
    }
    let has_prime = crew
        .iter()
        .any(|c| c.role.eq_ignore_ascii_case("prime"));
    if !has_prime {
        risks.push("No Prime hired — consider hiring a Prime to own Mandate strategy.".to_string());
    }
    if !hires.is_empty() {
        risks.push(format!(
            "{} role(s) need hiring — approving files pending hire requests that still need Clearance.",
            hires.len()
        ));
    }

    // Next actions the operator can approve.
    let mut next_actions = vec![format!(
        "Approve to create the Mandate \u{201c}{subject}\u{201d} + {} Brief(s).",
        briefs.len()
    )];
    if !hires.is_empty() {
        next_actions.push(format!(
            "Approve also files {} hire request(s) (pending Clearance) for the missing role(s).",
            hires.len()
        ));
    }
    let assignable = crew_slots.iter().filter(|s| s.have).count();
    if assignable > 0 {
        next_actions.push(format!(
            "{assignable} Brief track(s) can be assigned to existing active Operatives immediately."
        ));
    }
    // Honest end of the loop (company-model §12.5B): the Start-to-Shift step
    // is itself a governed gate — nothing runs until the operator starts it.
    next_actions.push(
        "Nothing runs automatically — after approving (and greenlighting any Clearances), \
         use Start the work to run the ready Briefs."
            .to_string(),
    );

    PrimeProposal {
        intent: intent.to_string(),
        summary,
        mandate_title: subject,
        mandate_brief: msg.to_string(),
        roles,
        crew: crew_slots,
        hires,
        briefs,
        risks,
        next_actions,
        ai_used: false,
        ai_status: AI_STATUS_DETERMINISTIC.to_string(),
    }
}

// ── Start-to-Shift (company-model §12.5B) ──────────────────────────────
//
// `prime.start` turns an APPROVED proposal's ready Briefs into real Shifts.
// The decision of WHICH created Briefs to start (and why each skipped one
// was skipped) is a PURE partition — testable without the run pipeline.

/// Why `prime.start` did not start a created Brief. Returned to the operator
/// so the loop is legible (what still needs a Clearance / a dependency).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedBrief {
    pub brief_id: String,
    pub reason: String,
}

/// Whether a created Brief can become a Shift right now. The handler derives
/// this from the canonical readiness query + the Brief card; the mapping to a
/// human reason lives here so it is unit-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartReadiness {
    /// Assigned to an active Operative, unblocked, not already claimed/running.
    Ready,
    /// No Operative assigned yet (a hire / Clearance is still pending).
    Unassigned,
    /// Blocked on a dependency Brief.
    Blocked,
    /// Already done / in review — nothing to start.
    Complete,
    /// Cancelled (terminal).
    Cancelled,
    /// The Brief no longer exists.
    Missing,
    /// Assigned but not startable right now (assignee inactive, or a Shift is
    /// already live on it).
    NotReady,
}

impl StartReadiness {
    /// `None` when the Brief should be started; otherwise the honest reason it
    /// is skipped.
    pub fn skip_reason(self) -> Option<&'static str> {
        match self {
            StartReadiness::Ready => None,
            StartReadiness::Unassigned => {
                Some("no Operative assigned yet — approve a Clearance / hire first")
            }
            StartReadiness::Blocked => Some("blocked on a dependency Brief"),
            StartReadiness::Complete => Some("already complete or in review"),
            StartReadiness::Cancelled => Some("cancelled"),
            StartReadiness::Missing => Some("Brief no longer exists"),
            StartReadiness::NotReady => {
                Some("not startable right now (assignee inactive or a Shift is already live)")
            }
        }
    }
}

/// Partition an approved proposal's created Briefs into the ones to start now
/// and the ones to skip with an honest reason. Order is preserved. PURE.
pub fn partition_start(briefs: &[(String, StartReadiness)]) -> (Vec<String>, Vec<SkippedBrief>) {
    let mut to_start = Vec::new();
    let mut skipped = Vec::new();
    for (id, readiness) in briefs {
        match readiness.skip_reason() {
            None => to_start.push(id.clone()),
            Some(reason) => skipped.push(SkippedBrief {
                brief_id: id.clone(),
                reason: reason.to_string(),
            }),
        }
    }
    (to_start, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(role: &str, status: &str) -> CrewMember {
        CrewMember {
            agent_id: format!("agt_{role}"),
            name: format!("{role}-1"),
            role: role.into(),
            status: status.into(),
        }
    }

    #[test]
    fn build_app_proposes_engineer_designer_tracks_and_integration() {
        let p = generate_proposal("Build me a web dashboard for sales", &[]);
        assert_eq!(p.intent, "build");
        assert!(p.roles.contains(&"engineer".to_string()));
        assert!(p.roles.contains(&"designer".to_string()));
        // role tracks + an integration brief (because >1 track).
        assert!(p.briefs.iter().any(|b| b.key == "track:engineer"));
        assert!(p.briefs.iter().any(|b| b.key == "track:designer"));
        let integ = p.briefs.iter().find(|b| b.key == "integrate").expect("integration brief");
        assert!(integ.depends_on.contains(&"track:engineer".to_string()));
        assert!(integ.depends_on.contains(&"track:designer".to_string()));
        // Title strips the leading imperative.
        assert_eq!(p.mandate_title, "Web dashboard for sales");
        assert!(!p.ai_used);
    }

    #[test]
    fn missing_roles_become_hire_suggestions_not_fake_agents() {
        // Only an active engineer exists → designer is a hire suggestion.
        let crew = vec![member("engineer", "active")];
        let p = generate_proposal("Build a web app", &crew);
        let eng = p.crew.iter().find(|s| s.role == "engineer").unwrap();
        assert!(eng.have, "existing engineer is matched");
        assert_eq!(eng.agent_id.as_deref(), Some("agt_engineer"));
        let des = p.crew.iter().find(|s| s.role == "designer").unwrap();
        assert!(!des.have, "no designer → not filled");
        assert!(p.hires.iter().any(|h| h.role == "designer"));
        assert!(!p.hires.iter().any(|h| h.role == "engineer"));
    }

    #[test]
    fn a_pending_operative_does_not_satisfy_a_role() {
        // A pending (inert) engineer must NOT count as filling the role.
        let crew = vec![member("engineer", "pending")];
        let p = generate_proposal("Build an API", &crew);
        let eng = p.crew.iter().find(|s| s.role == "engineer").unwrap();
        assert!(!eng.have);
        assert!(p.hires.iter().any(|h| h.role == "engineer"));
    }

    #[test]
    fn fix_and_research_intents_classify() {
        assert_eq!(generate_proposal("Fix the broken login bug", &[]).intent, "fix");
        assert_eq!(
            generate_proposal("Research the best auth provider", &[]).intent,
            "research"
        );
        assert_eq!(generate_proposal("Tidy up the kitchen", &[]).intent, "generic");
    }

    #[test]
    fn no_crew_surfaces_honest_risks() {
        let p = generate_proposal("Build a dashboard", &[]);
        assert!(p.risks.iter().any(|r| r.contains("No active Operatives")));
        assert!(p.risks.iter().any(|r| r.contains("No Prime")));
        // Nothing-runs-automatically is always the final next action.
        assert!(p.next_actions.last().unwrap().contains("Nothing runs automatically"));
    }

    #[test]
    fn single_role_request_has_no_integration_brief() {
        let p = generate_proposal("Write the onboarding docs", &[]);
        assert!(p.briefs.iter().any(|b| b.key == "track:writer"));
        assert!(!p.briefs.iter().any(|b| b.key == "integrate"));
    }

    // ── Prime Intelligence (company-model §12.5A): the breakdown SHAPE is
    //    intent-aware, and each Brief title carries the extracted subject. ──

    #[test]
    fn fix_intent_yields_reproduce_fix_verify_chain() {
        let p = generate_proposal("Fix the broken login bug", &[]);
        assert_eq!(p.intent, "fix");
        let keys: Vec<&str> = p.briefs.iter().map(|b| b.key.as_str()).collect();
        assert_eq!(keys, vec!["reproduce", "fix", "verify"]);
        // Sequential chain: fix depends on reproduce, verify depends on fix.
        let fix = p.briefs.iter().find(|b| b.key == "fix").unwrap();
        assert_eq!(fix.depends_on, vec!["reproduce".to_string()]);
        let verify = p.briefs.iter().find(|b| b.key == "verify").unwrap();
        assert_eq!(verify.depends_on, vec!["fix".to_string()]);
        // No parallel role tracks / integration Brief for a fix.
        assert!(!p.briefs.iter().any(|b| b.key.starts_with("track:")));
        assert!(!p.briefs.iter().any(|b| b.key == "integrate"));
        // Title carries the subject (deliverable-aware), not just a role.
        assert!(fix.title.contains("login"));
    }

    #[test]
    fn fix_with_qa_signal_routes_verify_to_qa() {
        // A QA signal in the request sends the verify Brief to a QA role.
        let p = generate_proposal("Fix the failing checkout test coverage", &[]);
        assert_eq!(p.intent, "fix");
        let verify = p.briefs.iter().find(|b| b.key == "verify").unwrap();
        assert_eq!(verify.role, "qa");
    }

    #[test]
    fn research_intent_yields_investigate_synthesize_chain() {
        let p = generate_proposal("Research the best auth provider", &[]);
        assert_eq!(p.intent, "research");
        let keys: Vec<&str> = p.briefs.iter().map(|b| b.key.as_str()).collect();
        assert_eq!(keys, vec!["investigate", "synthesize"]);
        let synth = p.briefs.iter().find(|b| b.key == "synthesize").unwrap();
        assert_eq!(synth.depends_on, vec!["investigate".to_string()]);
        assert!(synth.title.contains("auth provider"));
    }

    #[test]
    fn different_intents_yield_different_plan_shapes() {
        // The core "Prime Intelligence" promise: the plan is request-aware —
        // a build and a fix of the "same" noun produce DIFFERENT shapes.
        let build = generate_proposal("Build a payments dashboard", &[]);
        let fix = generate_proposal("Fix a payments dashboard bug", &[]);
        let build_keys: Vec<&str> = build.briefs.iter().map(|b| b.key.as_str()).collect();
        let fix_keys: Vec<&str> = fix.briefs.iter().map(|b| b.key.as_str()).collect();
        assert_ne!(build_keys, fix_keys);
        assert!(build.briefs.iter().any(|b| b.key == "integrate"));
        assert!(fix.briefs.iter().any(|b| b.key == "fix"));
    }

    #[test]
    fn hires_only_cover_roles_the_breakdown_uses() {
        // research → investigate/synthesize only needs a researcher; even
        // though "auth" hints engineer, no engineer Brief exists, so no
        // engineer is suggested (every hire maps to a Brief role).
        let p = generate_proposal("Research the best auth library", &[]);
        let used: std::collections::HashSet<&str> =
            p.briefs.iter().map(|b| b.role.as_str()).collect();
        for h in &p.hires {
            assert!(
                used.contains(h.role.as_str()),
                "suggested hire {} has no Brief in the plan",
                h.role
            );
        }
    }

    // ── Start-to-Shift partition (company-model §12.5B) ──

    #[test]
    fn partition_start_runs_only_ready_briefs_with_honest_skips() {
        let items = vec![
            ("b1".to_string(), StartReadiness::Ready),
            ("b2".to_string(), StartReadiness::Unassigned),
            ("b3".to_string(), StartReadiness::Blocked),
            ("b4".to_string(), StartReadiness::Ready),
            ("b5".to_string(), StartReadiness::Complete),
            ("b6".to_string(), StartReadiness::Cancelled),
            ("b7".to_string(), StartReadiness::NotReady),
            ("b8".to_string(), StartReadiness::Missing),
        ];
        let (to_start, skipped) = partition_start(&items);
        // Order preserved; only the Ready ones start.
        assert_eq!(to_start, vec!["b1".to_string(), "b4".to_string()]);
        // Every non-ready Brief is reported with a non-empty reason.
        assert_eq!(skipped.len(), 6);
        assert!(skipped.iter().all(|s| !s.reason.is_empty()));
        let unassigned = skipped.iter().find(|s| s.brief_id == "b2").unwrap();
        assert!(unassigned.reason.contains("no Operative"));
        let blocked = skipped.iter().find(|s| s.brief_id == "b3").unwrap();
        assert!(blocked.reason.contains("blocked"));
    }

    #[test]
    fn partition_start_with_nothing_ready_starts_nothing() {
        let items = vec![
            ("b1".to_string(), StartReadiness::Unassigned),
            ("b2".to_string(), StartReadiness::Blocked),
        ];
        let (to_start, skipped) = partition_start(&items);
        assert!(to_start.is_empty());
        assert_eq!(skipped.len(), 2);
    }
}
