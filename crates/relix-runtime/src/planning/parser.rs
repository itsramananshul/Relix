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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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

        PlanSpec {
            goal,
            constraints,
            success_criteria,
            preferred_agents,
            forbidden_agents,
            max_steps,
            budget_hint,
            original_spec: spec.to_string(),
        }
    }
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
}
