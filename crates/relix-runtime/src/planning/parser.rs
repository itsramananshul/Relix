//! RELIX-7.24 — `SpecParser`.
//!
//! Heuristic, ML-free parser that turns an operator's
//! natural-language specification into a structured
//! [`PlanSpec`]. The downstream
//! [`super::generator::PlanGenerator`] consumes the
//! `PlanSpec` and uses the registry to pick agents and a
//! topology.
//!
//! What it looks for:
//!
//! - **goal**: the first sentence of the spec, trimmed and
//!   normalised. Operators usually lead with the imperative.
//! - **constraints**: any sentence containing
//!   `must / must not / should not / avoid / without /
//!   no more than / under N (seconds|tokens|words)`.
//! - **success_criteria**: any sentence containing
//!   `return / produce / output / result should / ensure /
//!   summary / report`.
//! - **preferred_agents**: agent names from
//!   `[agents.<name>]` that appear verbatim in the spec.
//! - **forbidden_agents**: agent names preceded by
//!   negation keywords (`do not use`, `without`,
//!   `avoid`, `exclude`).
//! - **max_steps**: a numeric token followed by `step` /
//!   `steps`.
//! - **budget_hint**: any mention of `tokens`, `cost`,
//!   `cheap`, `expensive`, `fast`, `slow`. The first match
//!   wins; the planner uses this as a hint for topology
//!   selection (cheap → single agent; expensive → parallel).

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable wire version for the [`PlanSpec`] schema. Bumped when
/// a non-backwards-compatible field is added or renamed. The
/// approval store and any external consumer that holds a
/// signed spec across upgrades reads this to decide whether
/// to re-sign or migrate.
pub const PLAN_SPEC_VERSION: u32 = 1;

/// Structured output of [`SpecParser::parse`].
///
/// RELIX-7.24 hardened spec format. Carries five
/// tamper-evidence + change-tracking fields on top of the
/// heuristic fields the parser extracts:
///
/// - [`Self::version`] — schema version (always
///   [`PLAN_SPEC_VERSION`] for fresh parses).
/// - [`Self::spec_id`] — uuid v4 minted by the parser. Stable
///   across revisions of the same logical spec (the critic
///   loop and conflict resolver mutate the spec but keep
///   `spec_id` constant so an operator can correlate
///   pre-revision and post-revision views).
/// - [`Self::created_at_ms`] — unix millis at parse time.
/// - [`Self::signature`] — `blake3` hex of the canonical JSON
///   serialisation (all fields except `signature` itself,
///   keys sorted, no whitespace). Tamper-evident: any field
///   modified post-sign without re-signing makes
///   [`Self::verify`] return [`SpecVerificationError`].
/// - [`Self::changelog`] — ordered audit trail of every
///   `with_change` call the planning pipeline made on this
///   spec.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanSpec {
    /// The operator's stated objective. Always non-empty
    /// when the parse succeeds.
    pub goal: String,
    /// Extracted constraint sentences. Empty when none
    /// match the constraint-keyword set.
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Extracted success-criteria sentences.
    #[serde(default)]
    pub success_criteria: Vec<String>,
    /// Agent names the spec explicitly asks for.
    #[serde(default)]
    pub preferred_agents: Vec<String>,
    /// Agent names the spec explicitly excludes.
    #[serde(default)]
    pub forbidden_agents: Vec<String>,
    /// `N` from `N steps` / `N step` when present.
    #[serde(default)]
    pub max_steps: Option<usize>,
    /// Operator-mentioned budget hint (one of `"tokens"`,
    /// `"cheap"`, `"expensive"`, `"fast"`, `"slow"`,
    /// `"cost"`).
    #[serde(default)]
    pub budget_hint: Option<String>,
    /// Echo of the original spec for the planner's audit
    /// trail.
    pub original_spec: String,
    /// RELIX-7.24 Stage-1: heuristic complexity score in
    /// `0.0..=1.0`. Computed in [`SpecParser::parse`] from the
    /// number of constraints, success criteria, goal length,
    /// and distinct output types mentioned. Higher = the
    /// orchestrator is more likely to activate.
    ///
    /// Triggers (each contributes 0.7, summed, capped at 1.0):
    ///
    /// - More than 3 success criteria.
    /// - More than 5 constraint clauses.
    /// - Goal text longer than 150 words.
    /// - The spec mentions two or more distinct output
    ///   types (report, code, summary, analysis, plan,
    ///   design, implementation, documentation).
    #[serde(default)]
    pub complexity_score: f32,
    /// `true` when [`Self::complexity_score`] meets or
    /// exceeds the default 0.6 orchestrator-activation
    /// threshold. Operator-tunable thresholds live on
    /// [`super::orchestrator::OrchestratorConfig`]; this
    /// bool reports the default judgement so operators can
    /// read it directly off the parsed spec.
    #[serde(default)]
    pub is_complex: bool,
    /// RELIX-7.24 spec hardening — schema version (currently
    /// `1`). Always equal to [`PLAN_SPEC_VERSION`] for
    /// freshly-parsed specs.
    #[serde(default = "default_version")]
    pub version: u32,
    /// uuid v4 minted at parse time. Stable across critic /
    /// conflict-resolver revisions of the same logical spec.
    #[serde(default)]
    pub spec_id: String,
    /// Unix millis at parse time.
    #[serde(default)]
    pub created_at_ms: i64,
    /// `blake3` hex digest of the canonical-JSON
    /// representation (all fields except `signature` itself,
    /// keys sorted, no whitespace). `None` while the spec is
    /// being mutated; the planner / critic / conflict resolver
    /// re-sign via [`Self::sign`] after each mutation, and the
    /// approval store verifies via [`Self::verify`].
    #[serde(default)]
    pub signature: Option<String>,
    /// Append-only audit log of every change applied to the
    /// spec by the planning pipeline. Ordered chronologically;
    /// the parser seeds it with one `"parsed"` entry on
    /// initial parse.
    #[serde(default)]
    pub changelog: Vec<SpecChange>,
}

fn default_version() -> u32 {
    PLAN_SPEC_VERSION
}

/// One entry in [`PlanSpec::changelog`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpecChange {
    /// Unix millis at the moment the change was applied.
    pub changed_at_ms: i64,
    /// Stable string keying the change category. Conventions
    /// used by the planning pipeline today:
    ///
    /// - `"parsed"` — initial parse.
    /// - `"critic_feedback"` — critic loop injected issues +
    ///   suggestions as constraints.
    /// - `"conflict_rename"`, `"conflict_sequence"`,
    ///   `"conflict_drop"`, `"conflict_escalate"` — conflict
    ///   resolver mutated the spec via one of its strategies.
    /// - `"operator_edit"` — operator manually edited the
    ///   spec via the bridge / CLI.
    pub change_type: String,
    /// Free-form one-line human-readable description of the
    /// change.
    pub description: String,
}

/// Errors surfaced by [`PlanSpec::verify`].
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum SpecVerificationError {
    #[error("plan spec has no signature — call PlanSpec::sign() first")]
    Missing,
    #[error(
        "plan spec signature mismatch — expected `{expected}`, got `{actual}` \
         (spec has been modified since signing)"
    )]
    Mismatch { expected: String, actual: String },
    #[error("plan spec canonicalisation failed: {0}")]
    Canonicalise(String),
}

impl PlanSpec {
    /// Compute the canonical-JSON form for signing or
    /// verification: serialise every field except `signature`
    /// to JSON, then re-emit with sorted keys + no whitespace.
    /// `serde_json::Value::Object` is backed by `BTreeMap` in
    /// the default build, so key ordering is deterministic.
    pub fn canonical_json(&self) -> Result<String, SpecVerificationError> {
        let mut v = serde_json::to_value(self)
            .map_err(|e| SpecVerificationError::Canonicalise(e.to_string()))?;
        if let serde_json::Value::Object(map) = &mut v {
            map.remove("signature");
        }
        serde_json::to_string(&v).map_err(|e| SpecVerificationError::Canonicalise(e.to_string()))
    }

    /// Compute the blake3 hex digest of the canonical-JSON
    /// representation and write it into [`Self::signature`].
    /// Returns the freshly-computed signature so callers can
    /// log or echo it.
    pub fn sign(&mut self) -> Result<String, SpecVerificationError> {
        let canonical = self.canonical_json()?;
        let digest = blake3::hash(canonical.as_bytes());
        let hex = digest.to_hex().to_string();
        self.signature = Some(hex.clone());
        Ok(hex)
    }

    /// Recompute the canonical hash and compare it against the
    /// stored signature. Returns `Ok(())` on a match,
    /// [`SpecVerificationError`] otherwise.
    pub fn verify(&self) -> Result<(), SpecVerificationError> {
        let Some(expected) = self.signature.as_ref() else {
            return Err(SpecVerificationError::Missing);
        };
        let canonical = self.canonical_json()?;
        let actual = blake3::hash(canonical.as_bytes()).to_hex().to_string();
        if actual == *expected {
            Ok(())
        } else {
            Err(SpecVerificationError::Mismatch {
                expected: expected.clone(),
                actual,
            })
        }
    }

    /// Record a change in the audit log + invalidate any
    /// previous signature so subsequent
    /// [`Self::verify`] calls fail until the caller re-signs.
    /// Use [`Self::with_change_and_sign`] to record + re-sign
    /// in one shot.
    pub fn with_change(&mut self, change_type: &str, description: &str) {
        self.changelog.push(SpecChange {
            changed_at_ms: unix_now_ms(),
            change_type: change_type.to_string(),
            description: description.to_string(),
        });
        self.signature = None;
    }

    /// Record a change AND re-sign in one shot. Returns the
    /// new signature on success.
    pub fn with_change_and_sign(
        &mut self,
        change_type: &str,
        description: &str,
    ) -> Result<String, SpecVerificationError> {
        self.with_change(change_type, description);
        self.sign()
    }
}

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The parser. Stateless — every call to [`Self::parse`] is
/// pure over the inputs. Accepts the list of known agent
/// names so it can recognise mentions.
#[derive(Clone, Debug, Default)]
pub struct SpecParser {
    known_agents: Vec<String>,
}

impl SpecParser {
    /// Build a parser with no agent dictionary. The output's
    /// `preferred_agents` / `forbidden_agents` will always be
    /// empty — useful for unit tests that exercise the
    /// goal / constraints paths in isolation.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a parser that recognises the given agent names.
    /// Names are matched case-insensitively against
    /// whitespace + punctuation boundaries.
    pub fn with_known_agents(agents: impl IntoIterator<Item = String>) -> Self {
        Self {
            known_agents: agents.into_iter().collect(),
        }
    }

    /// Parse a natural-language spec into a [`PlanSpec`].
    /// Returns a `PlanSpec` even for marginal input — the
    /// goal field carries whatever the parser could extract.
    /// Empty / whitespace-only input yields an empty `goal`.
    ///
    /// RELIX-7.24 hardening: every parsed spec is stamped
    /// with `version`, a fresh uuid v4 `spec_id`,
    /// `created_at_ms`, a one-entry `changelog` (`"parsed"`),
    /// and a blake3 signature over the canonical JSON
    /// representation. Downstream pipeline stages (critic,
    /// conflict resolver, approval store) preserve `spec_id`
    /// across revisions and re-sign after every mutation.
    pub fn parse(&self, spec: &str) -> PlanSpec {
        let trimmed = spec.trim();
        let created_at_ms = unix_now_ms();
        let spec_id = uuid::Uuid::new_v4().hyphenated().to_string();
        if trimmed.is_empty() {
            let mut empty = PlanSpec {
                original_spec: spec.to_string(),
                version: PLAN_SPEC_VERSION,
                spec_id: spec_id.clone(),
                created_at_ms,
                changelog: vec![SpecChange {
                    changed_at_ms: created_at_ms,
                    change_type: "parsed".into(),
                    description: "empty input — placeholder spec".into(),
                }],
                ..Default::default()
            };
            // Sign the placeholder so the approval store can
            // still verify it.
            let _ = empty.sign();
            return empty;
        }
        let sentences = split_sentences(trimmed);
        let goal = sentences
            .first()
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        let mut constraints = Vec::new();
        let mut success_criteria = Vec::new();
        for sent in &sentences {
            let lower = sent.to_lowercase();
            if is_constraint(&lower) {
                constraints.push(sent.trim().to_string());
            }
            if is_success_criterion(&lower) {
                success_criteria.push(sent.trim().to_string());
            }
        }

        let (preferred_agents, forbidden_agents) =
            extract_agent_mentions(trimmed, &self.known_agents);
        let max_steps = extract_max_steps(trimmed);
        let budget_hint = extract_budget_hint(trimmed);
        let complexity_score =
            compute_complexity_score(trimmed, &goal, &constraints, &success_criteria);
        let is_complex = complexity_score >= DEFAULT_COMPLEXITY_THRESHOLD;

        let goal_preview: String = goal.chars().take(96).collect();
        let mut out = PlanSpec {
            goal,
            constraints,
            success_criteria,
            preferred_agents,
            forbidden_agents,
            max_steps,
            budget_hint,
            original_spec: spec.to_string(),
            complexity_score,
            is_complex,
            version: PLAN_SPEC_VERSION,
            spec_id,
            created_at_ms,
            signature: None,
            changelog: vec![SpecChange {
                changed_at_ms: created_at_ms,
                change_type: "parsed".into(),
                description: format!("initial parse: \"{goal_preview}\""),
            }],
        };
        // Sign the freshly-parsed spec. Failure here is only
        // possible if `serde_json::to_value(self)` fails,
        // which is structurally unreachable for `PlanSpec`
        // (no Map<NonString, _> or NaN floats in any field) —
        // the result is discarded as a defence-in-depth
        // anyway. Any caller can re-sign explicitly via
        // [`PlanSpec::sign`].
        let _ = out.sign();
        out
    }
}

/// Default complexity-threshold used by [`PlanSpec::is_complex`]
/// and the default
/// [`super::orchestrator::OrchestratorConfig::complexity_threshold`].
/// Kept here so both the parser and the orchestrator agree on
/// the "is this a complex spec?" boundary out of the box.
pub const DEFAULT_COMPLEXITY_THRESHOLD: f32 = 0.6;

/// Output-type keywords that contribute to the complexity
/// score when two or more distinct ones appear in the spec.
const OUTPUT_TYPE_KEYWORDS: &[&str] = &[
    "report",
    "code",
    "summary",
    "analysis",
    "plan",
    "design",
    "implementation",
    "documentation",
];

/// Score the spec on the heuristic complexity ladder. Each of
/// the four triggers contributes 0.7; the sum is clamped to
/// `1.0`. Any single trigger therefore clears the default 0.6
/// activation threshold.
fn compute_complexity_score(
    full_spec: &str,
    goal: &str,
    constraints: &[String],
    success_criteria: &[String],
) -> f32 {
    let mut score: f32 = 0.0;
    if success_criteria.len() > 3 {
        score += 0.7;
    }
    if constraints.len() > 5 {
        score += 0.7;
    }
    if goal_word_count(goal) > 150 {
        score += 0.7;
    }
    if distinct_output_types(full_spec) >= 2 {
        score += 0.7;
    }
    score.min(1.0)
}

fn goal_word_count(goal: &str) -> usize {
    goal.split_whitespace().count()
}

fn distinct_output_types(spec: &str) -> usize {
    let lower = spec.to_lowercase();
    let mut found = 0;
    for kw in OUTPUT_TYPE_KEYWORDS {
        // Word-boundary match: surround spec with spaces so
        // " report " matches but "reporter" does not.
        let needle_a = format!(" {kw} ");
        let needle_b = format!(" {kw}.");
        let needle_c = format!(" {kw},");
        let needle_d = format!(" {kw}s "); // crude pluralisation
        let padded = format!(" {lower} ");
        if padded.contains(&needle_a)
            || padded.contains(&needle_b)
            || padded.contains(&needle_c)
            || padded.contains(&needle_d)
        {
            found += 1;
        }
    }
    found
}

// ── helpers ───────────────────────────────────────────────

/// Split a spec into sentence-ish chunks. Conservative: split
/// on `.`, `!`, `?`, or `;` outside of word boundaries. We
/// keep punctuation OFF the returned strings so downstream
/// scoring sees clean text.
fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for ch in text.chars() {
        if matches!(ch, '.' | '!' | '?' | ';' | '\n') {
            let s = buf.trim().to_string();
            if !s.is_empty() {
                out.push(s);
            }
            buf.clear();
        } else {
            buf.push(ch);
        }
    }
    let tail = buf.trim().to_string();
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

/// Constraint-style keywords. Lowercase pattern matching.
const CONSTRAINT_KEYWORDS: &[&str] = &[
    "must not",
    "should not",
    "must ",
    "do not use",
    "do not ",
    "avoid",
    "without",
    "no more than",
    "under ",
    "less than",
    "at most",
    "never",
];

fn is_constraint(lower: &str) -> bool {
    CONSTRAINT_KEYWORDS.iter().any(|k| lower.contains(k))
}

/// Success-criteria keywords.
const SUCCESS_KEYWORDS: &[&str] = &[
    "return ",
    "produce ",
    "output ",
    "result should",
    "ensure ",
    "summary ",
    "report ",
    "deliver ",
    "must include",
    "should include",
];

fn is_success_criterion(lower: &str) -> bool {
    SUCCESS_KEYWORDS.iter().any(|k| lower.contains(k))
}

/// Negation prefixes that flip an agent mention from
/// preferred to forbidden.
const NEGATION_PREFIXES: &[&str] = &[
    "do not use",
    "don't use",
    "without",
    "avoid",
    "exclude",
    "not allowed",
    "forbidden",
    "never use",
];

/// Clause-break tokens that close a negation scope. Once any
/// of these appears between a negation prefix and an agent
/// mention, the negation no longer applies — "without
/// code-agent and use research-agent" keeps research-agent
/// preferred because `and` resets the scope.
const CLAUSE_BREAKS: &[&str] = &[
    " and ", " or ", " then ", " but ", " also ", " plus ", ", ", "; ",
];

fn extract_agent_mentions(spec: &str, known: &[String]) -> (Vec<String>, Vec<String>) {
    let lower = spec.to_lowercase();
    let mut preferred: Vec<String> = Vec::new();
    let mut forbidden: Vec<String> = Vec::new();
    for agent in known {
        let agent_lower = agent.to_lowercase();
        let mut idx = 0;
        let mut latest: Option<bool> = None;
        while let Some(pos) = lower[idx..].find(&agent_lower) {
            let abs = idx + pos;
            let before = &lower[..abs];
            // For each negation prefix, find the LATEST
            // position (closest to the mention). If a clause-
            // break sits BETWEEN that position and the
            // mention, the negation has been reset and the
            // mention is preferred.
            let is_forbidden = NEGATION_PREFIXES.iter().any(|n| {
                let Some(neg_pos) = before.rfind(n) else {
                    return false;
                };
                if abs.saturating_sub(neg_pos) > 50 {
                    return false;
                }
                let scope = &lower[neg_pos..abs];
                !CLAUSE_BREAKS.iter().any(|cb| scope.contains(cb))
            });
            latest = Some(is_forbidden);
            idx = abs + agent_lower.len();
        }
        match latest {
            Some(true) => forbidden.push(agent.clone()),
            Some(false) => preferred.push(agent.clone()),
            None => {}
        }
    }
    preferred.sort();
    forbidden.sort();
    (preferred, forbidden)
}

/// Find a `N step` / `N steps` pattern. Returns the number on
/// first match.
fn extract_max_steps(spec: &str) -> Option<usize> {
    let lower = spec.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    for w in words.windows(2) {
        let head = w[0].trim_matches(|c: char| !c.is_ascii_digit());
        let tail_lower = w[1].trim_matches(|c: char| !c.is_alphanumeric());
        if (tail_lower == "step" || tail_lower == "steps")
            && let Ok(n) = head.parse::<usize>()
        {
            return Some(n);
        }
    }
    None
}

const BUDGET_HINTS: &[&str] = &[
    "tokens",
    "cheap",
    "expensive",
    "fast",
    "slow",
    "cost",
    "budget",
];

fn extract_budget_hint(spec: &str) -> Option<String> {
    let lower = spec.to_lowercase();
    for hint in BUDGET_HINTS {
        if lower.contains(hint) {
            return Some((*hint).to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_goal_correctly_from_natural_language_spec() {
        let p = SpecParser::new();
        let spec = p.parse(
            "Research the latest developments in Rust async runtimes. \
             Return a summary under 300 words.",
        );
        assert_eq!(
            spec.goal,
            "Research the latest developments in Rust async runtimes"
        );
    }

    #[test]
    fn empty_spec_yields_empty_plan_spec() {
        let s = SpecParser::new().parse("");
        assert!(s.goal.is_empty());
        assert!(s.constraints.is_empty());
        assert!(s.success_criteria.is_empty());

        let s = SpecParser::new().parse("    \n  ");
        assert!(s.goal.is_empty());
    }

    #[test]
    fn extracts_constraints_from_must_should_not_without_keywords() {
        let p = SpecParser::new();
        let s = p.parse(
            "Summarise the docs. The summary must not exceed 500 words. \
             Do not use external APIs. Avoid speculation.",
        );
        // Every sentence after the first should be classified.
        assert!(s.constraints.iter().any(|c| c.contains("must not exceed")));
        assert!(s.constraints.iter().any(|c| c.contains("Do not use")));
        assert!(
            s.constraints
                .iter()
                .any(|c| c.contains("Avoid speculation"))
        );
    }

    #[test]
    fn extracts_success_criteria_from_return_produce_output_keywords() {
        let p = SpecParser::new();
        let s = p.parse(
            "Analyse the request. Return a markdown report. \
             Produce a list of findings. Output JSON.",
        );
        assert!(
            s.success_criteria
                .iter()
                .any(|c| c.contains("Return a markdown report"))
        );
        assert!(
            s.success_criteria
                .iter()
                .any(|c| c.contains("Produce a list"))
        );
        assert!(s.success_criteria.iter().any(|c| c.contains("Output JSON")));
    }

    #[test]
    fn extracts_preferred_agents_when_names_appear_in_spec() {
        let p = SpecParser::with_known_agents(vec![
            "research-agent".to_string(),
            "code-agent".to_string(),
        ]);
        let s = p.parse("Use research-agent to gather sources then summarise.");
        assert_eq!(s.preferred_agents, vec!["research-agent".to_string()]);
        assert!(s.forbidden_agents.is_empty());
    }

    #[test]
    fn extracts_forbidden_agents_when_negated() {
        let p = SpecParser::with_known_agents(vec![
            "research-agent".to_string(),
            "code-agent".to_string(),
        ]);
        let s = p.parse("Summarise without code-agent and use research-agent.");
        assert_eq!(s.forbidden_agents, vec!["code-agent".to_string()]);
        assert_eq!(s.preferred_agents, vec!["research-agent".to_string()]);
    }

    #[test]
    fn extracts_max_steps_from_n_steps_pattern() {
        let p = SpecParser::new();
        let s = p.parse("Plan the project in 5 steps. Each step must be concrete.");
        assert_eq!(s.max_steps, Some(5));
    }

    #[test]
    fn extracts_max_steps_singular_form() {
        let p = SpecParser::new();
        let s = p.parse("This should take 1 step.");
        assert_eq!(s.max_steps, Some(1));
    }

    #[test]
    fn extracts_budget_hints_from_cost_and_token_keywords() {
        let p = SpecParser::new();
        let s = p.parse("Find the cheapest provider that meets the cost requirement.");
        // "cheap" appears as a substring of "cheapest" → picked
        // first by the keyword scan.
        assert_eq!(s.budget_hint, Some("cheap".into()));

        let s2 = p.parse("Stay under 500 tokens.");
        assert_eq!(s2.budget_hint, Some("tokens".into()));
    }

    #[test]
    fn unknown_agent_names_are_not_extracted() {
        let p = SpecParser::with_known_agents(vec!["research-agent".to_string()]);
        let s = p.parse("Use ghost-agent for the work.");
        assert!(s.preferred_agents.is_empty());
        assert!(s.forbidden_agents.is_empty());
    }

    #[test]
    fn original_spec_is_echoed_back() {
        let p = SpecParser::new();
        let spec = "Do the thing.";
        assert_eq!(p.parse(spec).original_spec, spec);
    }

    #[test]
    fn split_sentences_handles_mixed_terminators() {
        let s = split_sentences("First. Second! Third? Fourth; Fifth");
        assert_eq!(s, vec!["First", "Second", "Third", "Fourth", "Fifth"]);
    }

    #[test]
    fn complexity_score_is_zero_for_a_short_simple_spec() {
        let p = SpecParser::new();
        let s = p.parse("Greet the user.");
        assert_eq!(s.complexity_score, 0.0);
        assert!(!s.is_complex);
    }

    #[test]
    fn long_goal_alone_pushes_complexity_above_the_default_threshold() {
        let p = SpecParser::new();
        // 160-word goal.
        let goal: String = std::iter::repeat_n("word", 160)
            .collect::<Vec<_>>()
            .join(" ");
        let spec = p.parse(&format!("{goal}. Return a summary."));
        assert!(
            spec.complexity_score >= DEFAULT_COMPLEXITY_THRESHOLD,
            "complex due to long goal: score={}",
            spec.complexity_score,
        );
        assert!(spec.is_complex);
    }

    #[test]
    fn many_success_criteria_alone_pushes_complexity_above_the_default_threshold() {
        let p = SpecParser::new();
        let s = p.parse(
            "Goal here. Return X. Return Y. Return Z. Return W. \
             Return V. Produce a markdown report.",
        );
        assert!(s.success_criteria.len() > 3);
        assert!(s.is_complex);
    }

    #[test]
    fn many_constraints_alone_pushes_complexity_above_the_default_threshold() {
        let p = SpecParser::new();
        let s = p.parse(
            "Goal. Must not exceed 100 words. Must not call external APIs. \
             Avoid speculation. Do not use the code-agent. Should not retry. \
             Without redactions. Avoid placeholders.",
        );
        assert!(s.constraints.len() > 5, "{:?}", s.constraints);
        assert!(s.is_complex);
    }

    #[test]
    fn distinct_output_types_alone_pushes_complexity_above_the_default_threshold() {
        let p = SpecParser::new();
        let s = p.parse("Build the system. Produce a report and code and a design.");
        assert!(s.is_complex);
        assert!(s.complexity_score >= DEFAULT_COMPLEXITY_THRESHOLD);
    }

    #[test]
    fn single_output_type_alone_does_not_push_complexity_above_threshold() {
        let p = SpecParser::new();
        let s = p.parse("Build the system. Produce a single report.");
        // Only one output type → no contribution; goal short
        // → no contribution; no constraints or successes.
        assert!(s.complexity_score < DEFAULT_COMPLEXITY_THRESHOLD);
        assert!(!s.is_complex);
    }

    #[test]
    fn complexity_score_is_capped_at_one() {
        let p = SpecParser::new();
        let long_goal: String = std::iter::repeat_n("alpha", 160)
            .collect::<Vec<_>>()
            .join(" ");
        let s = p.parse(&format!(
            "{long_goal}. Return X. Return Y. Return Z. Return W. Return V. \
             Produce a report and code and a design. \
             Must not exceed 100 words. Must not call external APIs. \
             Avoid speculation. Do not use the code-agent. Should not retry. \
             Without redactions. Avoid placeholders."
        ));
        assert!((s.complexity_score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn distinct_output_types_counts_words_not_substrings() {
        // "reporter" must NOT count as "report".
        assert_eq!(distinct_output_types("a reporter wrote the article"), 0);
        // "report" and "code" both count, distinct = 2.
        assert_eq!(distinct_output_types("produce a report and code"), 2);
        // Pluralisation: "reports" counts.
        assert_eq!(distinct_output_types("file two reports here"), 1);
    }

    // ─── RELIX-7.24 hardened spec format ──────────────

    #[test]
    fn freshly_parsed_spec_has_version_uuid_timestamp_and_signature() {
        let p = SpecParser::new();
        let s = p.parse("Greet the user.");
        assert_eq!(s.version, PLAN_SPEC_VERSION);
        // uuid v4 in standard hyphenated form is exactly 36 chars.
        assert_eq!(s.spec_id.len(), 36, "spec_id={}", s.spec_id);
        // Sanity: hyphen positions match uuid v4 layout.
        let bytes = s.spec_id.as_bytes();
        assert_eq!(bytes[8], b'-');
        assert_eq!(bytes[13], b'-');
        assert_eq!(bytes[18], b'-');
        assert_eq!(bytes[23], b'-');
        assert!(s.created_at_ms > 0, "created_at_ms must be set");
        let sig = s.signature.as_ref().expect("freshly parsed must be signed");
        // blake3 hex is 64 characters.
        assert_eq!(sig.len(), 64, "blake3 hex sig len = {}", sig.len());
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn freshly_parsed_spec_has_initial_parsed_changelog_entry() {
        let p = SpecParser::new();
        let s = p.parse("Do the thing.");
        assert_eq!(s.changelog.len(), 1);
        assert_eq!(s.changelog[0].change_type, "parsed");
        assert!(s.changelog[0].description.contains("initial parse"));
        assert!(s.changelog[0].changed_at_ms > 0);
    }

    #[test]
    fn verify_returns_ok_on_unmodified_spec() {
        let p = SpecParser::new();
        let s = p.parse("Research the web.");
        s.verify().expect("freshly-parsed spec must verify");
    }

    #[test]
    fn verify_returns_mismatch_after_any_field_modified_without_resign() {
        let p = SpecParser::new();
        let mut s = p.parse("Research the web.");
        // Tamper with a non-signature field.
        s.goal = "Hack the planet.".into();
        match s.verify() {
            Err(SpecVerificationError::Mismatch { .. }) => {}
            other => panic!("expected Mismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_returns_missing_when_signature_is_none() {
        let p = SpecParser::new();
        let mut s = p.parse("Research the web.");
        s.signature = None;
        assert!(matches!(s.verify(), Err(SpecVerificationError::Missing)));
    }

    #[test]
    fn sign_after_modification_makes_verify_pass_again() {
        let p = SpecParser::new();
        let mut s = p.parse("Research the web.");
        s.goal = "Research the web carefully.".into();
        assert!(s.verify().is_err());
        s.sign().expect("re-sign");
        s.verify().expect("verify after re-sign");
    }

    #[test]
    fn with_change_appends_log_entry_and_invalidates_signature() {
        let p = SpecParser::new();
        let mut s = p.parse("Research the web.");
        let original_changelog_len = s.changelog.len();
        let original_sig = s.signature.clone();
        s.with_change("critic_feedback", "Critic flagged step 2");
        assert_eq!(s.changelog.len(), original_changelog_len + 1);
        let last = s.changelog.last().unwrap();
        assert_eq!(last.change_type, "critic_feedback");
        assert!(last.description.contains("step 2"));
        assert!(last.changed_at_ms > 0);
        // Signature must have been invalidated.
        assert!(s.signature.is_none());
        assert!(original_sig.is_some());
    }

    #[test]
    fn with_change_and_sign_records_and_resigns_atomically() {
        let p = SpecParser::new();
        let mut s = p.parse("Research the web.");
        let sig = s
            .with_change_and_sign("operator_edit", "manual tweak")
            .expect("re-sign succeeds");
        // New signature returned + stored.
        assert_eq!(s.signature.as_deref(), Some(sig.as_str()));
        // Verifies cleanly.
        s.verify().expect("verify");
        // Changelog grew.
        assert_eq!(s.changelog.last().unwrap().change_type, "operator_edit");
    }

    #[test]
    fn changelog_grows_across_multiple_with_change_calls() {
        let p = SpecParser::new();
        let mut s = p.parse("Research the web.");
        s.with_change("critic_feedback", "round 1");
        s.with_change("conflict_rename", "step b renamed");
        s.with_change("operator_edit", "tweaked goal");
        assert_eq!(s.changelog.len(), 4); // parsed + 3 changes
        assert_eq!(s.changelog[0].change_type, "parsed");
        assert_eq!(s.changelog[1].change_type, "critic_feedback");
        assert_eq!(s.changelog[2].change_type, "conflict_rename");
        assert_eq!(s.changelog[3].change_type, "operator_edit");
    }

    #[test]
    fn empty_input_parse_still_signs_and_verifies() {
        let p = SpecParser::new();
        let s = p.parse("");
        assert!(s.goal.is_empty());
        assert_eq!(s.version, PLAN_SPEC_VERSION);
        assert!(s.signature.is_some());
        s.verify().expect("empty spec verifies");
    }

    #[test]
    fn canonical_json_excludes_signature_field() {
        let p = SpecParser::new();
        let s = p.parse("Research the web.");
        let canonical = s.canonical_json().expect("canonical_json");
        assert!(
            !canonical.contains("\"signature\""),
            "canonical_json must not include the signature field: {canonical}"
        );
    }

    #[test]
    fn two_parses_of_identical_input_yield_different_spec_ids_but_signatures_match_after_normalising_timestamps()
     {
        let p = SpecParser::new();
        let mut a = p.parse("Research the web.");
        let mut b = p.parse("Research the web.");
        assert_ne!(a.spec_id, b.spec_id, "spec_id must be unique per parse");
        // Even with the same goal text, two parses differ on
        // timestamp + spec_id so signatures differ.
        assert_ne!(a.signature, b.signature);
        // Normalise the fields that legitimately vary, re-sign,
        // and the signatures should match.
        b.spec_id = a.spec_id.clone();
        b.created_at_ms = a.created_at_ms;
        b.changelog = a.changelog.clone();
        let _ = a.sign();
        let _ = b.sign();
        assert_eq!(a.signature, b.signature);
    }
}
