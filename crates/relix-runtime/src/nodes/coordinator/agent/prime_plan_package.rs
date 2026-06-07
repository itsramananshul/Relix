//! Prime Plan-Package Authoring v1 — opt-in, constrained model authoring of a
//! *proposed* Brief decomposition plan
//! (`relix-execution-and-issue-design` §1.7/§1.8/§3.1 — the planner pattern /
//! plan package, here driven autonomously by the Prime tick).
//!
//! **THE MODEL IS NOT THE PERMISSION SYSTEM.** This module lets an opt-in model
//! author only the *content* of a plan package — the plan title, the plan body,
//! the approval summary, and a bounded list of proposed child Briefs (title /
//! priority / a backward `after` dependency). It never approves a gate: the
//! authored content is still opened through the EXISTING
//! `TaskStore::open_plan_package` primitive, which creates an immutable `plan`
//! Dossier revision + a `suggest_tasks` proposal + an approval-bound `confirm`
//! and leaves the confirm **open** for a human to approve (only acceptance
//! materializes the child Briefs, through the existing exactly-once decomposition
//! ledger). The model may NOT choose methods/capabilities/tools, assign agents
//! (children always open unassigned — assignee hints are deliberately omitted,
//! exactly like the manual companion composer), mutate an existing Dossier, or
//! approve anything. Its reply is fully re-validated + sanitized + secret-redacted
//! server-side before it is opened, and any malformed / overlong / unavailable
//! output degrades to a deterministic safe decomposition with an honest mode.
//!
//! This module is PURE and dependency-light (snapshot → prompt → validate), so the
//! prompt builder and the validator are fully unit-tested without a mesh or a
//! provider. The live mesh `ai.chat` wiring + the deterministic fallback that
//! bounds it live in `prime_driver`, which owns the eligibility checks and the
//! `open_plan_package` plumbing.

use serde::Deserialize;

use crate::nodes::coordinator::agent::prime_plan::sanitize_text;
use crate::nodes::coordinator::brief::{self, MAX_CHILD_TITLE_LEN, is_priority};

/// Hard cap on the prompt we hand the model — bounds cost and keeps the request
/// tight (a bounded snapshot only, never a repo / file / secret dump).
pub const MAX_PLAN_PACKAGE_PROMPT_CHARS: usize = 2000;
/// Hard cap on the raw model output we will even attempt to validate. A larger
/// blob is rejected outright (→ deterministic fallback) rather than processed.
pub const MAX_PLAN_PACKAGE_OUTPUT_CHARS: usize = 16 * 1024;
/// Most proposed child Briefs an autonomous plan package may carry. Tighter than
/// the store's [`brief::MAX_SUGGESTED_CHILDREN`] cap (20) — an autonomous,
/// unattended decomposition stays small and reviewable. Extra children are
/// dropped (never an error).
pub const MAX_AUTONOMOUS_CHILDREN: usize = 8;
/// Plan-Dossier title cap (chars).
pub const MAX_PLAN_TITLE: usize = 120;
/// Plan-Dossier body cap (chars).
pub const MAX_PLAN_BODY: usize = 4000;
/// Approval-summary cap (chars). Mirrors the store's proposal-summary cap.
pub const MAX_PLAN_SUMMARY: usize = brief::MAX_PROPOSAL_SUMMARY_LEN;

/// The fixed approval prompt the bound `confirm` carries. The model authors the
/// plan/summary/children, NOT the approval prompt — keeping the gate language
/// constant and unspoofable.
pub const APPROVAL_PROMPT: &str =
    "Approve this plan and create the proposed task(s)? Nothing is created until you approve.";

/// How a single plan package's content was authored — surfaced on the tick
/// record so the operator sees the provenance instead of guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimePlanPackageMode {
    /// Model plan-package authoring is off (the env flag is unset). The
    /// autonomous tick does not author a plan package at all in this mode; the
    /// variant exists for symmetry with the other Prime authoring modes.
    DeterministicOnly,
    /// The model returned a valid, bounded, sanitized plan package that was used.
    LlmUsed,
    /// The model answered but its output was empty / overlong / unsafe / malformed,
    /// so the deterministic decomposition was used instead.
    Fallback,
    /// The model could not be reached (no decider / mesh / AI peer, or the call
    /// failed), so the deterministic decomposition was used.
    Unavailable,
}

impl PrimePlanPackageMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PrimePlanPackageMode::DeterministicOnly => "deterministic_only",
            PrimePlanPackageMode::LlmUsed => "llm_used",
            PrimePlanPackageMode::Fallback => "fallback",
            PrimePlanPackageMode::Unavailable => "unavailable",
        }
    }
}

/// A validated, sanitized plan package ready to hand to
/// `TaskStore::open_plan_package`. The children are already normalized enough that
/// the store's own [`brief::normalize_proposal`] accepts them (defence in depth:
/// the store re-validates regardless).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPlanPackage {
    pub plan_title: String,
    pub plan_body: String,
    pub summary: String,
    pub prompt: String,
    pub children: Vec<brief::ChildSpec>,
}

/// The bounded, secret-free snapshot the model authors a plan package from. Built
/// from the target Brief's own title + status + the owning Mandate's title —
/// never any secret, credential, token, repo content, or large free-text dump.
#[derive(Debug, Clone)]
pub struct PrimePlanPackageSnapshot {
    pub brief_title: String,
    pub brief_status: String,
    pub mandate_title: String,
}

impl PrimePlanPackageSnapshot {
    pub fn new(brief_title: &str, brief_status: &str, mandate_title: &str) -> Self {
        let brief_title = match brief_title.trim() {
            "" => "(untitled Brief)".to_string(),
            t => t.to_string(),
        };
        let brief_status = match brief_status.trim() {
            "" => "todo".to_string(),
            s => s.to_string(),
        };
        let mandate_title = match mandate_title.trim() {
            "" => "(untitled Mandate)".to_string(),
            t => t.to_string(),
        };
        Self {
            brief_title,
            brief_status,
            mandate_title,
        }
    }
}

// ── Raw wire shape (lenient; every field optional) ────────────────────────

#[derive(Debug, Default, Deserialize)]
struct RawChild {
    #[serde(default)]
    title: String,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    after: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPlanPackage {
    #[serde(default)]
    plan_title: String,
    #[serde(default)]
    plan_body: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    children: Vec<RawChild>,
}

/// Strip a single leading/trailing markdown code fence (```json … ``` or
/// ``` … ```) if present, returning the inner body. Leaves un-fenced input
/// untouched. (A model often wraps its JSON in one fence.)
fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let rest = match rest.find('\n') {
        Some(nl) => &rest[nl + 1..],
        None => rest,
    };
    rest.trim()
        .strip_suffix("```")
        .map_or(rest.trim(), str::trim)
}

/// Build the bounded, sanitized plan-package-authoring prompt. PURE + unit-tested.
/// The model is told to decompose the target Brief into a small, concrete tree of
/// child Briefs and to emit STRICT JSON only. Because the coordinator re-validates
/// and re-gates everything (the package is only ever *opened* for approval, never
/// approved), the prompt only needs to steer — it is never trusted.
pub fn build_plan_package_prompt(snap: &PrimePlanPackageSnapshot) -> String {
    let raw = format!(
        "You are Prime, a company planning lead. Decompose the Brief below into a SMALL, concrete \
plan of child Briefs (sub-tasks). This is a DRAFT proposal only: it is opened for a human to \
approve, and nothing is created until they approve.\n\
Rules:\n\
- Output STRICT JSON only — a single object, no prose, no markdown, no code fences.\n\
- Shape: {{\"plan_title\": string, \"plan_body\": string (plain markdown), \"summary\": string, \
\"children\": [{{\"title\": string, \"priority\": one of low|normal|high|urgent, \"after\": \
integer index of an EARLIER child this one depends on (optional)}}]}}.\n\
- Propose between 1 and {max} children. Keep each title short and concrete.\n\
- `after` must reference only an EARLIER child by its 0-based position (a backward dependency); \
omit it for independent work.\n\
- Do NOT assign people, pick tools/methods, or include secrets, credentials, tokens, file \
contents, or shell/tool commands.\n\
- Do NOT invent facts beyond what is given below.\n\n\
Brief to plan:\n\
- title: {brief_title}\n\
- status: {brief_status}\n\
- mandate: {mandate_title}\n",
        max = MAX_AUTONOMOUS_CHILDREN,
        brief_title = snap.brief_title,
        brief_status = snap.brief_status,
        mandate_title = snap.mandate_title,
    );
    raw.chars().take(MAX_PLAN_PACKAGE_PROMPT_CHARS).collect()
}

/// Validate + sanitize a raw model plan-package reply into a [`ValidatedPlanPackage`].
/// STRICT: rejects empty / overlong output and any reply that yields no plan body
/// or no usable child; otherwise strips a surrounding code fence, parses the JSON,
/// secret-redacts + bounds every string, drops empty / over-cap children (down to
/// [`MAX_AUTONOMOUS_CHILDREN`]), keeps only valid priorities, and remaps each
/// `after` to a strictly-earlier kept sibling (dropping any forward / self /
/// unknown / dropped-target reference). On any rejection the caller falls back to
/// the deterministic decomposition. PURE + unit-tested.
///
/// `brief_title` (already safe) seeds the plan title / summary when the model
/// omits them — never blended into the model's child list.
pub fn validate_plan_package(raw: &str, brief_title: &str) -> Result<ValidatedPlanPackage, String> {
    if raw.chars().count() > MAX_PLAN_PACKAGE_OUTPUT_CHARS {
        return Err("plan-package output too long".to_string());
    }
    let body = strip_code_fence(raw).trim();
    if body.is_empty() {
        return Err("empty plan-package output".to_string());
    }
    let parsed: RawPlanPackage = serde_json::from_str(body)
        .map_err(|e| format!("plan-package output was not valid JSON: {e}"))?;

    // Plan body: the model's, else reject (a plan package needs a plan body — we
    // do not invent one here; the caller's deterministic fallback supplies it).
    let plan_body = sanitize_text(&parsed.plan_body, MAX_PLAN_BODY);
    if plan_body.is_empty() {
        return Err("plan-package output had no plan body".to_string());
    }

    // Plan title: the model's, else derived from the Brief title.
    let plan_title = {
        let t = sanitize_text(&parsed.plan_title, MAX_PLAN_TITLE);
        if t.is_empty() {
            sanitize_text(&format!("Plan — {brief_title}"), MAX_PLAN_TITLE)
        } else {
            t
        }
    };

    // Summary: the model's, else a safe derived one.
    let summary = {
        let s = sanitize_text(&parsed.summary, MAX_PLAN_SUMMARY);
        if s.is_empty() {
            sanitize_text(
                &format!("Proposed decomposition of {brief_title}"),
                MAX_PLAN_SUMMARY,
            )
        } else {
            s
        }
    };

    let children = normalize_children(&parsed.children);
    if children.is_empty() {
        return Err("plan-package output had no usable child Brief".to_string());
    }

    Ok(ValidatedPlanPackage {
        plan_title,
        plan_body,
        summary,
        prompt: APPROVAL_PROMPT.to_string(),
        children,
    })
}

/// Sanitize + bound the model's raw children into store-acceptable [`brief::ChildSpec`]s:
/// drop empty titles, cap the count, keep only valid priorities, and remap each
/// `after` onto a strictly-earlier KEPT sibling (else drop the dependency). Assignee
/// hints are never set — an autonomous decomposition opens its children unassigned.
fn normalize_children(raw: &[RawChild]) -> Vec<brief::ChildSpec> {
    // First pass: keep non-empty, bounded titles + valid priorities, remembering
    // each kept child's ORIGINAL index so `after` can be remapped.
    let mut kept: Vec<(usize, brief::ChildSpec)> = Vec::new();
    for (orig, rc) in raw.iter().enumerate() {
        if kept.len() >= MAX_AUTONOMOUS_CHILDREN {
            break;
        }
        let title = sanitize_text(&rc.title, MAX_CHILD_TITLE_LEN);
        if title.is_empty() {
            continue;
        }
        let priority = match rc.priority.as_deref().map(str::trim) {
            Some(p) if !p.is_empty() && is_priority(p) => Some(p.to_string()),
            _ => None,
        };
        kept.push((
            orig,
            brief::ChildSpec {
                title,
                priority,
                after: None,
                assignee_agent_id: None,
                assignee_role: None,
            },
        ));
    }

    // Map original index → new (kept) index, so an `after` that referenced a
    // dropped child resolves to nothing and is discarded.
    let max_orig = raw.len();
    let mut new_of_orig: Vec<Option<usize>> = vec![None; max_orig];
    for (new_i, (orig, _)) in kept.iter().enumerate() {
        if let Some(slot) = new_of_orig.get_mut(*orig) {
            *slot = Some(new_i);
        }
    }

    // Second pass: resolve each kept child's `after` against the kept set, keeping
    // ONLY a strictly-earlier sibling (a backward edge, so the graph stays acyclic).
    let resolved: Vec<Option<usize>> = kept
        .iter()
        .enumerate()
        .map(|(new_i, (orig, _))| {
            raw[*orig].after.and_then(|a| {
                new_of_orig
                    .get(a)
                    .copied()
                    .flatten()
                    .filter(|&mapped| mapped < new_i)
            })
        })
        .collect();

    kept.into_iter()
        .zip(resolved)
        .map(|((_, mut spec), after)| {
            spec.after = after;
            spec
        })
        .collect()
}

/// A safe, deterministic decomposition used when model authoring is unavailable or
/// its output is rejected (mirrors the strategy/orchestration deterministic
/// fallbacks). A generic plan / build / verify chain derived only from the Brief
/// title — no secrets, no invented facts, no assignees. The body carries the
/// "DRAFT / not approved" governance language. PURE.
pub fn deterministic_plan_package(brief_title: &str) -> ValidatedPlanPackage {
    let title = match brief_title.trim() {
        "" => "this Brief".to_string(),
        t => sanitize_text(t, MAX_PLAN_TITLE),
    };
    let child = |t: &str, after: Option<usize>, priority: &str| brief::ChildSpec {
        title: sanitize_text(t, MAX_CHILD_TITLE_LEN),
        priority: Some(priority.to_string()),
        after,
        assignee_agent_id: None,
        assignee_role: None,
    };
    let children = vec![
        child(&format!("Plan: {title}"), None, "normal"),
        child(&format!("Build: {title}"), Some(0), "normal"),
        child(&format!("Verify: {title}"), Some(1), "normal"),
    ];
    let plan_body = format!(
        "# Plan — {title}\n\
         \n\
         This is a Prime DRAFT decomposition proposal (deterministic v1); it is NOT approved. A \
         human must approve the bound confirm before any child Brief is created; rejecting it \
         stops the work here.\n\
         \n\
         ## Approach\n\
         1. **Plan** — break the work down and confirm the approach.\n\
         2. **Build** — do the work (depends on the plan).\n\
         3. **Verify** — review and wrap up (depends on the build).\n",
    );
    ValidatedPlanPackage {
        plan_title: sanitize_text(&format!("Plan — {title}"), MAX_PLAN_TITLE),
        plan_body: sanitize_text(&plan_body, MAX_PLAN_BODY),
        summary: sanitize_text(
            &format!("Proposed decomposition of {title} into 3 step(s)"),
            MAX_PLAN_SUMMARY,
        ),
        prompt: APPROVAL_PROMPT.to_string(),
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRIEF: &str = "Ship the login page";

    fn snap() -> PrimePlanPackageSnapshot {
        PrimePlanPackageSnapshot::new(BRIEF, "todo", "Auth Mandate")
    }

    fn valid_json() -> String {
        serde_json::json!({
            "plan_title": "Login page plan",
            "plan_body": "# Plan\n\nBreak the login page into steps.",
            "summary": "Decompose the login page",
            "children": [
                {"title": "Wire the form", "priority": "high"},
                {"title": "Hook up auth", "priority": "normal", "after": 0},
                {"title": "Add tests", "priority": "low", "after": 1}
            ]
        })
        .to_string()
    }

    #[test]
    fn prompt_is_bounded_and_steers() {
        let p = build_plan_package_prompt(&snap());
        assert!(p.chars().count() <= MAX_PLAN_PACKAGE_PROMPT_CHARS);
        assert!(p.contains("Ship the login page"));
        assert!(p.contains("Auth Mandate"));
        assert!(p.contains("JSON"));
        assert!(p.contains("children"));
    }

    #[test]
    fn prompt_is_clamped_for_a_huge_title() {
        let big = "y".repeat(50_000);
        let s = PrimePlanPackageSnapshot::new(&big, "todo", "M");
        let p = build_plan_package_prompt(&s);
        assert!(p.chars().count() <= MAX_PLAN_PACKAGE_PROMPT_CHARS);
    }

    #[test]
    fn accepts_a_well_formed_plan_package() {
        let vp = validate_plan_package(&valid_json(), BRIEF).expect("good json accepted");
        assert_eq!(vp.plan_title, "Login page plan");
        assert!(vp.plan_body.contains("Break the login page"));
        assert_eq!(vp.children.len(), 3);
        assert_eq!(vp.children[0].after, None);
        assert_eq!(vp.children[1].after, Some(0));
        assert_eq!(vp.children[2].after, Some(1));
        assert_eq!(vp.children[0].priority.as_deref(), Some("high"));
        // No assignee hints are ever set.
        assert!(vp.children.iter().all(|c| c.assignee_agent_id.is_none()));
        assert!(vp.children.iter().all(|c| c.assignee_role.is_none()));
        assert_eq!(vp.prompt, APPROVAL_PROMPT);
    }

    #[test]
    fn rejects_empty_and_overlong_and_malformed() {
        assert!(validate_plan_package("   ", BRIEF).is_err());
        assert!(validate_plan_package("not json", BRIEF).is_err());
        let big = "x".repeat(MAX_PLAN_PACKAGE_OUTPUT_CHARS + 1);
        assert!(validate_plan_package(&big, BRIEF).is_err());
    }

    #[test]
    fn rejects_missing_body_or_no_children() {
        let no_body = serde_json::json!({
            "plan_title": "T",
            "children": [{"title": "a"}]
        })
        .to_string();
        assert!(validate_plan_package(&no_body, BRIEF).is_err());
        let no_children = serde_json::json!({
            "plan_body": "do it",
            "children": []
        })
        .to_string();
        assert!(validate_plan_package(&no_children, BRIEF).is_err());
        // Children present but all empty-titled → no usable child.
        let empty_children = serde_json::json!({
            "plan_body": "do it",
            "children": [{"title": "   "}, {"title": ""}]
        })
        .to_string();
        assert!(validate_plan_package(&empty_children, BRIEF).is_err());
    }

    #[test]
    fn caps_child_count() {
        let many: Vec<_> = (0..MAX_AUTONOMOUS_CHILDREN + 5)
            .map(|i| serde_json::json!({"title": format!("t{i}")}))
            .collect();
        let j = serde_json::json!({"plan_body": "b", "children": many}).to_string();
        let vp = validate_plan_package(&j, BRIEF).unwrap();
        assert_eq!(vp.children.len(), MAX_AUTONOMOUS_CHILDREN);
    }

    #[test]
    fn drops_invalid_priority_and_forward_or_unknown_after() {
        let j = serde_json::json!({
            "plan_body": "b",
            "children": [
                {"title": "a", "priority": "ludicrous"},   // invalid → None
                {"title": "b", "after": 5},                 // unknown → dropped
                {"title": "c", "after": 2},                 // forward (self/after self) → dropped
                {"title": "d", "after": 0}                  // valid backward → kept
            ]
        })
        .to_string();
        let vp = validate_plan_package(&j, BRIEF).unwrap();
        assert_eq!(vp.children.len(), 4);
        assert_eq!(vp.children[0].priority, None);
        assert_eq!(vp.children[1].after, None);
        assert_eq!(vp.children[2].after, None);
        assert_eq!(vp.children[3].after, Some(0));
    }

    #[test]
    fn remaps_after_across_dropped_children() {
        // child index 0 is dropped (empty title); a later child's `after: 1`
        // (which referenced the kept child now at new index 0) must remap to 0.
        let j = serde_json::json!({
            "plan_body": "b",
            "children": [
                {"title": "   "},              // dropped
                {"title": "keep-a"},           // new index 0
                {"title": "keep-b", "after": 1} // referenced orig 1 → new 0
            ]
        })
        .to_string();
        let vp = validate_plan_package(&j, BRIEF).unwrap();
        assert_eq!(vp.children.len(), 2);
        assert_eq!(vp.children[0].title, "keep-a");
        assert_eq!(vp.children[1].after, Some(0));
    }

    #[test]
    fn strips_code_fence_and_redacts_secrets() {
        let secret = "sk-abcdef012345678901234567890";
        let raw = format!(
            "```json\n{}\n```",
            serde_json::json!({
                "plan_title": format!("deploy with {secret}"),
                "plan_body": format!("token {secret} here"),
                "children": [{"title": format!("use {secret}")}]
            })
        );
        let vp = validate_plan_package(&raw, BRIEF).unwrap();
        assert!(
            !vp.plan_title.contains("sk-abcdef"),
            "title: {}",
            vp.plan_title
        );
        assert!(!vp.plan_body.contains("sk-abcdef"));
        assert!(!vp.children[0].title.contains("sk-abcdef"));
    }

    #[test]
    fn deterministic_is_safe_and_bounded() {
        let vp = deterministic_plan_package(BRIEF);
        assert_eq!(vp.children.len(), 3);
        assert_eq!(vp.children[0].after, None);
        assert_eq!(vp.children[1].after, Some(0));
        assert_eq!(vp.children[2].after, Some(1));
        assert!(vp.plan_body.to_ascii_lowercase().contains("not approved"));
        assert!(vp.plan_title.chars().count() <= MAX_PLAN_TITLE);
        assert!(vp.plan_body.chars().count() <= MAX_PLAN_BODY);
        assert!(vp.children.iter().all(|c| c.assignee_agent_id.is_none()));
        assert_eq!(vp.prompt, APPROVAL_PROMPT);
    }

    #[test]
    fn mode_strings_are_stable() {
        assert_eq!(
            PrimePlanPackageMode::DeterministicOnly.as_str(),
            "deterministic_only"
        );
        assert_eq!(PrimePlanPackageMode::LlmUsed.as_str(), "llm_used");
        assert_eq!(PrimePlanPackageMode::Fallback.as_str(), "fallback");
        assert_eq!(PrimePlanPackageMode::Unavailable.as_str(), "unavailable");
    }
}
