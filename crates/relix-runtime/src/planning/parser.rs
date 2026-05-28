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

/// Structured output of [`SpecParser::parse`].
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
    pub fn parse(&self, spec: &str) -> PlanSpec {
        let trimmed = spec.trim();
        if trimmed.is_empty() {
            return PlanSpec {
                original_spec: spec.to_string(),
                ..Default::default()
            };
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

        PlanSpec {
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
        }
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
}
