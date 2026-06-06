//! **Prime model-plan validator** (company-model §12.5A — the seam).
//!
//! The deterministic planner in [`super::prime`] is the rule-based
//! *interpretation* step. §12.5A locks the seam: a future model may replace
//! that interpretation while reusing the identical governed `prime.approve` /
//! `prime.start` execution path. This module is that plug-point.
//!
//! [`validate_model_plan`] is a PURE function that takes the RAW text a model
//! emitted (a JSON plan), and either returns a [`ValidatedPlan`] — bounded,
//! sanitized, secret-redacted, with unique Brief keys and dependency edges that
//! only reference known keys and contain no cycle — or a [`PlanValidationError`]
//! the caller turns into an honest fallback reason. It NEVER mutates anything,
//! NEVER calls a model, and NEVER trusts a single field verbatim: every string
//! is run through the same secret redaction the rest of the spine uses and
//! bounded to a hard length, every role is collapsed to a canonical family, and
//! the dependency graph is validated for unknown/self edges and cycles.
//!
//! The coordinator is the AUTHORITATIVE validator: even when a (less-trusted)
//! bridge supplies the model output, `prime.propose` runs it through here
//! server-side before anything is persisted. A model can therefore never inject
//! an oversized blob, a secret-shaped value, a dangling dependency, or a cyclic
//! Brief graph into the governed plan.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;

use super::prime::{ProposedBrief, canon_role};

// ── Hard bounds (a model cannot exceed any of these) ──────────────────────

/// Largest raw model output we will even parse. 16 KiB is far more than a
/// structured plan needs and bounds the parse + sanitize cost.
pub const MAX_MODEL_OUTPUT_BYTES: usize = 16 * 1024;
/// Most Briefs a single plan may contain.
pub const MAX_BRIEFS: usize = 16;
/// Most dependency edges one Brief may declare.
pub const MAX_DEPS_PER_BRIEF: usize = 16;
/// Most risks we keep (extras are dropped, never an error).
pub const MAX_RISKS: usize = 12;
/// Mandate title cap (mirrors the deterministic `bound_title`).
pub const MAX_TITLE: usize = 80;
/// Brief title cap.
pub const MAX_BRIEF_TITLE: usize = 120;
/// Mandate brief / description cap.
pub const MAX_BRIEF_DESC: usize = 2000;
/// One risk line cap.
pub const MAX_RISK: usize = 240;
/// Brief key cap.
pub const MAX_KEY: usize = 48;

/// Why a model plan was rejected. The `Display` text is shown to the operator
/// (via `ai_status`) as the honest reason the deterministic planner was used
/// instead — so it must never leak raw model content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanValidationError {
    /// Empty / whitespace-only model output.
    Empty,
    /// Output exceeded [`MAX_MODEL_OUTPUT_BYTES`].
    TooLarge { bytes: usize },
    /// The output was not valid JSON for the plan contract.
    Parse(String),
    /// No usable Mandate title (and none derivable from the request).
    MissingTitle,
    /// The plan contained no Briefs.
    NoBriefs,
    /// More than [`MAX_BRIEFS`] Briefs.
    TooManyBriefs { count: usize },
    /// A Brief had an empty key after normalization.
    EmptyKey,
    /// A Brief had an empty title after sanitization.
    EmptyBriefTitle { key: String },
    /// Two Briefs normalized to the same key.
    DuplicateKey { key: String },
    /// A `depends_on` referenced a key no Brief defines.
    UnknownDependency { key: String, dep: String },
    /// A Brief depended on itself.
    SelfDependency { key: String },
    /// The dependency edges form a cycle (would deadlock the board).
    DependencyCycle,
}

impl std::fmt::Display for PlanValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanValidationError::Empty => write!(f, "model output was empty"),
            PlanValidationError::TooLarge { bytes } => {
                write!(f, "model output too large ({bytes} bytes, max {MAX_MODEL_OUTPUT_BYTES})")
            }
            PlanValidationError::Parse(e) => write!(f, "model output was not valid plan JSON: {e}"),
            PlanValidationError::MissingTitle => write!(f, "plan had no usable Mandate title"),
            PlanValidationError::NoBriefs => write!(f, "plan contained no Briefs"),
            PlanValidationError::TooManyBriefs { count } => {
                write!(f, "plan had too many Briefs ({count}, max {MAX_BRIEFS})")
            }
            PlanValidationError::EmptyKey => write!(f, "a Brief had an empty key"),
            PlanValidationError::EmptyBriefTitle { key } => {
                write!(f, "Brief \"{key}\" had an empty title")
            }
            PlanValidationError::DuplicateKey { key } => write!(f, "duplicate Brief key \"{key}\""),
            PlanValidationError::UnknownDependency { key, dep } => {
                write!(f, "Brief \"{key}\" depends on unknown key \"{dep}\"")
            }
            PlanValidationError::SelfDependency { key } => {
                write!(f, "Brief \"{key}\" depends on itself")
            }
            PlanValidationError::DependencyCycle => write!(f, "Brief dependencies form a cycle"),
        }
    }
}

impl std::error::Error for PlanValidationError {}

/// A validated, sanitized plan ready to be turned into a [`super::prime::PrimeProposal`]
/// via [`super::prime::proposal_from_model`]. Crew matching, hire suggestions,
/// and governance risks are NOT here — those stay coordinator-authoritative and
/// are computed from the live roster, never from the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPlan {
    /// One of `build` / `fix` / `research` / `generic`.
    pub intent: String,
    pub summary: String,
    pub mandate_title: String,
    pub mandate_brief: String,
    pub briefs: Vec<ProposedBrief>,
    /// Model-surfaced risks (sanitized, bounded). Governance risks are added
    /// later by the finalizer.
    pub risks: Vec<String>,
}

// ── Raw wire shape (lenient; every field optional) ────────────────────────

#[derive(Debug, Default, Deserialize)]
struct RawBrief {
    #[serde(default)]
    key: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    depends_on: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPlan {
    #[serde(default)]
    intent: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    mandate_title: String,
    #[serde(default)]
    mandate_brief: String,
    #[serde(default)]
    briefs: Vec<RawBrief>,
    #[serde(default)]
    risks: Vec<String>,
}

/// Collapse a free-form intent string into the canonical set. Unknown → `generic`.
fn canon_intent(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "build" | "create" | "make" | "ship" | "implement" => "build",
        "fix" | "bug" | "debug" | "repair" => "fix",
        "research" | "investigate" | "explore" | "analyze" | "analyse" => "research",
        _ => "generic",
    }
}

/// Sanitize one free-form string for storage: redact secret-shaped values,
/// drop control characters, collapse whitespace, and bound the length on a
/// char boundary. PURE.
pub fn sanitize_text(s: &str, max: usize) -> String {
    // 1) Redact secret-shaped tokens with the same masker the spine uses.
    let redacted = crate::rig::redact_secrets(s, "");
    // 2) Drop control chars (newlines/tabs become spaces) and collapse runs.
    let mut out = String::with_capacity(redacted.len());
    let mut last_space = false;
    for c in redacted.chars() {
        // Control chars, the BOM, and any whitespace all collapse to a single
        // separating space; everything else is kept verbatim.
        if c.is_control() || c == '\u{feff}' || c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
    }
    let trimmed = out.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let bounded: String = trimmed.chars().take(max).collect();
    // Prefer a word boundary when truncating.
    match bounded.rfind(' ') {
        Some(i) if i > max / 4 => bounded[..i].trim_end().to_string(),
        _ => bounded.trim_end().to_string(),
    }
}

/// Normalize a Brief key to a stable slug: lowercase, keep `[a-z0-9:_-]`,
/// every other run becomes a single `-`. Bounded to [`MAX_KEY`].
fn normalize_key(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_KEY));
    let mut last_dash = false;
    for c in raw.trim().to_ascii_lowercase().chars() {
        if c.is_ascii_alphanumeric() || c == ':' || c == '_' || c == '-' {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
        if out.chars().count() >= MAX_KEY {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

/// Detect a cycle in the dependency graph (keys → their deps). Returns `true`
/// if any cycle exists. Iterative DFS with a recursion stack colour map.
fn has_cycle(adj: &HashMap<String, Vec<String>>) -> bool {
    // 0 = unvisited, 1 = on stack, 2 = done.
    let mut state: HashMap<&str, u8> = HashMap::new();
    for start in adj.keys() {
        if state.get(start.as_str()).copied().unwrap_or(0) != 0 {
            continue;
        }
        // (node, child-index) explicit stack.
        let mut stack: Vec<(&str, usize)> = vec![(start.as_str(), 0)];
        state.insert(start.as_str(), 1);
        while let Some(&mut (node, ref mut idx)) = stack.last_mut() {
            let deps = adj.get(node);
            let next = deps.and_then(|d| d.get(*idx));
            match next {
                Some(dep) => {
                    *idx += 1;
                    match state.get(dep.as_str()).copied().unwrap_or(0) {
                        1 => return true, // back-edge → cycle
                        0 => {
                            state.insert(dep.as_str(), 1);
                            stack.push((dep.as_str(), 0));
                        }
                        _ => {}
                    }
                }
                None => {
                    state.insert(node, 2);
                    stack.pop();
                }
            }
        }
    }
    false
}

/// Validate + sanitize raw model output into a [`ValidatedPlan`]. PURE.
///
/// `original_message` (already secret-redacted by the caller) is used only as a
/// fallback for a missing Mandate title / brief — never blended into the model's
/// claims.
pub fn validate_model_plan(
    raw_json: &str,
    original_message: &str,
) -> Result<ValidatedPlan, PlanValidationError> {
    if raw_json.len() > MAX_MODEL_OUTPUT_BYTES {
        return Err(PlanValidationError::TooLarge {
            bytes: raw_json.len(),
        });
    }
    let trimmed = raw_json.trim();
    if trimmed.is_empty() {
        return Err(PlanValidationError::Empty);
    }
    let raw: RawPlan =
        serde_json::from_str(trimmed).map_err(|e| PlanValidationError::Parse(e.to_string()))?;

    let intent = canon_intent(&raw.intent).to_string();

    // Title: model's, else derived from the request, else reject.
    let mut mandate_title = sanitize_text(&raw.mandate_title, MAX_TITLE);
    if mandate_title.is_empty() {
        mandate_title = sanitize_text(original_message, MAX_TITLE);
    }
    if mandate_title.is_empty() {
        return Err(PlanValidationError::MissingTitle);
    }

    // Brief / description: model's, else the request.
    let mut mandate_brief = sanitize_text(&raw.mandate_brief, MAX_BRIEF_DESC);
    if mandate_brief.is_empty() {
        mandate_brief = sanitize_text(original_message, MAX_BRIEF_DESC);
    }

    let summary = {
        let s = sanitize_text(&raw.summary, MAX_TITLE);
        if s.is_empty() {
            match intent.as_str() {
                "build" => format!("Build: {mandate_title}"),
                "fix" => format!("Fix: {mandate_title}"),
                "research" => format!("Research: {mandate_title}"),
                _ => format!("Work: {mandate_title}"),
            }
        } else {
            s
        }
    };

    if raw.briefs.is_empty() {
        return Err(PlanValidationError::NoBriefs);
    }
    if raw.briefs.len() > MAX_BRIEFS {
        return Err(PlanValidationError::TooManyBriefs {
            count: raw.briefs.len(),
        });
    }

    // First pass: normalize keys + titles + roles, enforce uniqueness.
    let mut briefs: Vec<ProposedBrief> = Vec::with_capacity(raw.briefs.len());
    let mut seen: HashSet<String> = HashSet::new();
    for rb in &raw.briefs {
        let key = normalize_key(&rb.key);
        if key.is_empty() {
            return Err(PlanValidationError::EmptyKey);
        }
        if !seen.insert(key.clone()) {
            return Err(PlanValidationError::DuplicateKey { key });
        }
        let title = sanitize_text(&rb.title, MAX_BRIEF_TITLE);
        if title.is_empty() {
            return Err(PlanValidationError::EmptyBriefTitle { key });
        }
        let role = canon_role(&rb.role).to_string();
        briefs.push(ProposedBrief {
            key,
            title,
            role,
            depends_on: Vec::new(),
        });
    }

    // Second pass: normalize + validate dependency edges against known keys.
    let known: HashSet<String> = briefs.iter().map(|b| b.key.clone()).collect();
    let raw_by_index: Vec<&RawBrief> = raw.briefs.iter().collect();
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for (i, b) in briefs.iter_mut().enumerate() {
        let mut deps: Vec<String> = Vec::new();
        let mut dep_seen: HashSet<String> = HashSet::new();
        for d in raw_by_index[i].depends_on.iter().take(MAX_DEPS_PER_BRIEF) {
            let dep = normalize_key(d);
            if dep.is_empty() {
                continue;
            }
            if dep == b.key {
                return Err(PlanValidationError::SelfDependency { key: b.key.clone() });
            }
            if !known.contains(dep.as_str()) {
                return Err(PlanValidationError::UnknownDependency {
                    key: b.key.clone(),
                    dep,
                });
            }
            if dep_seen.insert(dep.clone()) {
                deps.push(dep);
            }
        }
        adj.insert(b.key.clone(), deps.clone());
        b.depends_on = deps;
    }

    if has_cycle(&adj) {
        return Err(PlanValidationError::DependencyCycle);
    }

    // Risks: sanitize, bound count, drop empties.
    let risks: Vec<String> = raw
        .risks
        .iter()
        .map(|r| sanitize_text(r, MAX_RISK))
        .filter(|r| !r.is_empty())
        .take(MAX_RISKS)
        .collect();

    Ok(ValidatedPlan {
        intent,
        summary,
        mandate_title,
        mandate_brief,
        briefs,
        risks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQ: &str = "Build a payments dashboard";

    fn valid_json() -> String {
        serde_json::json!({
            "intent": "build",
            "summary": "Build the payments dashboard",
            "mandate_title": "Payments dashboard",
            "mandate_brief": "A dashboard showing payment flows.",
            "briefs": [
                {"key": "api", "title": "Payments API", "role": "engineer", "depends_on": []},
                {"key": "ui", "title": "Dashboard UI", "role": "designer", "depends_on": []},
                {"key": "integrate", "title": "Integrate + ship", "role": "engineer",
                 "depends_on": ["api", "ui"]}
            ],
            "risks": ["payment data is sensitive"]
        })
        .to_string()
    }

    #[test]
    fn accepts_a_well_formed_plan() {
        let p = validate_model_plan(&valid_json(), REQ).unwrap();
        assert_eq!(p.intent, "build");
        assert_eq!(p.mandate_title, "Payments dashboard");
        assert_eq!(p.briefs.len(), 3);
        let integ = p.briefs.iter().find(|b| b.key == "integrate").unwrap();
        assert_eq!(integ.depends_on, vec!["api".to_string(), "ui".to_string()]);
        assert_eq!(p.risks, vec!["payment data is sensitive".to_string()]);
    }

    #[test]
    fn rejects_oversized_output() {
        let big = "x".repeat(MAX_MODEL_OUTPUT_BYTES + 1);
        assert!(matches!(
            validate_model_plan(&big, REQ),
            Err(PlanValidationError::TooLarge { .. })
        ));
    }

    #[test]
    fn rejects_empty_and_malformed() {
        assert_eq!(validate_model_plan("   ", REQ), Err(PlanValidationError::Empty));
        assert!(matches!(
            validate_model_plan("not json", REQ),
            Err(PlanValidationError::Parse(_))
        ));
    }

    #[test]
    fn rejects_empty_and_too_many_briefs() {
        let none = serde_json::json!({"mandate_title": "X", "briefs": []}).to_string();
        assert_eq!(validate_model_plan(&none, REQ), Err(PlanValidationError::NoBriefs));
        let many: Vec<_> = (0..MAX_BRIEFS + 1)
            .map(|i| serde_json::json!({"key": format!("k{i}"), "title": format!("t{i}"), "role": "engineer"}))
            .collect();
        let j = serde_json::json!({"mandate_title": "X", "briefs": many}).to_string();
        assert!(matches!(
            validate_model_plan(&j, REQ),
            Err(PlanValidationError::TooManyBriefs { .. })
        ));
    }

    #[test]
    fn rejects_duplicate_keys() {
        let j = serde_json::json!({
            "mandate_title": "X",
            "briefs": [
                {"key": "Do It!", "title": "a", "role": "engineer"},
                {"key": "do-it", "title": "b", "role": "engineer"}
            ]
        })
        .to_string();
        // Both normalize to "do-it".
        assert!(matches!(
            validate_model_plan(&j, REQ),
            Err(PlanValidationError::DuplicateKey { .. })
        ));
    }

    #[test]
    fn rejects_unknown_and_self_dependencies() {
        let unknown = serde_json::json!({
            "mandate_title": "X",
            "briefs": [{"key": "a", "title": "a", "role": "engineer", "depends_on": ["ghost"]}]
        })
        .to_string();
        assert!(matches!(
            validate_model_plan(&unknown, REQ),
            Err(PlanValidationError::UnknownDependency { .. })
        ));
        let me = serde_json::json!({
            "mandate_title": "X",
            "briefs": [{"key": "a", "title": "a", "role": "engineer", "depends_on": ["a"]}]
        })
        .to_string();
        assert!(matches!(
            validate_model_plan(&me, REQ),
            Err(PlanValidationError::SelfDependency { .. })
        ));
    }

    #[test]
    fn rejects_dependency_cycles() {
        let j = serde_json::json!({
            "mandate_title": "X",
            "briefs": [
                {"key": "a", "title": "a", "role": "engineer", "depends_on": ["b"]},
                {"key": "b", "title": "b", "role": "engineer", "depends_on": ["c"]},
                {"key": "c", "title": "c", "role": "engineer", "depends_on": ["a"]}
            ]
        })
        .to_string();
        assert_eq!(
            validate_model_plan(&j, REQ),
            Err(PlanValidationError::DependencyCycle)
        );
    }

    #[test]
    fn strips_secret_shaped_values_from_every_field() {
        let secret = "sk-abcdef012345678901234567890";
        let j = serde_json::json!({
            "mandate_title": format!("Deploy with {secret}"),
            "mandate_brief": format!("token AKIAIOSFODNN7EXAMPLE here"),
            "briefs": [{"key": "a", "title": format!("use {secret}"), "role": "engineer"}],
            "risks": [format!("leaked {secret}")]
        })
        .to_string();
        let p = validate_model_plan(&j, REQ).unwrap();
        assert!(!p.mandate_title.contains("sk-abcdef"), "title: {}", p.mandate_title);
        assert!(p.mandate_title.contains("***"));
        assert!(!p.briefs[0].title.contains("sk-abcdef"));
        assert!(!p.risks[0].contains("sk-abcdef"));
    }

    #[test]
    fn normalizes_roles_and_intent() {
        let j = serde_json::json!({
            "intent": "totally-unknown",
            "mandate_title": "X",
            "briefs": [
                {"key": "a", "title": "a", "role": "backend"},
                {"key": "b", "title": "b", "role": "ux"},
                {"key": "c", "title": "c", "role": "weird-role"}
            ]
        })
        .to_string();
        let p = validate_model_plan(&j, REQ).unwrap();
        assert_eq!(p.intent, "generic");
        assert_eq!(p.briefs[0].role, "engineer"); // backend → engineer
        assert_eq!(p.briefs[1].role, "designer"); // ux → designer
        assert_eq!(p.briefs[2].role, "engineer"); // unknown → engineer (safe default)
    }

    #[test]
    fn falls_back_to_request_for_missing_title() {
        let j = serde_json::json!({
            "briefs": [{"key": "a", "title": "do a thing", "role": "engineer"}]
        })
        .to_string();
        let p = validate_model_plan(&j, "Investigate the outage").unwrap();
        assert_eq!(p.mandate_title, "Investigate the outage");
    }

    #[test]
    fn bounds_overlong_titles() {
        let long = "word ".repeat(60);
        let j = serde_json::json!({
            "mandate_title": long,
            "briefs": [{"key": "a", "title": "a", "role": "engineer"}]
        })
        .to_string();
        let p = validate_model_plan(&j, REQ).unwrap();
        assert!(p.mandate_title.chars().count() <= MAX_TITLE);
    }

    #[test]
    fn drops_empty_risks_and_caps_count() {
        let risks: Vec<_> = (0..MAX_RISKS + 5).map(|i| format!("risk {i}")).collect();
        let mut risks_with_empty = vec![String::new(), "   ".to_string()];
        risks_with_empty.extend(risks);
        let j = serde_json::json!({
            "mandate_title": "X",
            "briefs": [{"key": "a", "title": "a", "role": "engineer"}],
            "risks": risks_with_empty
        })
        .to_string();
        let p = validate_model_plan(&j, REQ).unwrap();
        assert!(p.risks.len() <= MAX_RISKS);
        assert!(p.risks.iter().all(|r| !r.is_empty()));
    }
}
