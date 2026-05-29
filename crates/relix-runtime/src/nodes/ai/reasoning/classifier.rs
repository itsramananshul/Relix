//! GAP 16 Component 1 — complexity classifier.
//!
//! Pure-function rules classifier that turns a chat prompt
//! into a [`ReasoningTier`]. The classifier is deliberately
//! NOT an LLM call — putting an LLM on every hot-path
//! classification would defeat the cost-saving purpose of
//! the tier router. Instead we score the prompt on a small
//! set of cheap structural signals:
//!
//! - **Length** — prompts over 600 chars usually need more
//!   capable models.
//! - **Code / math keywords** — prompts that mention
//!   `function`, `algorithm`, `derive`, ` regression`, etc.
//!   typically need Medium-or-Complex tier.
//! - **Ambiguity / decision markers** — `decide`, `evaluate`,
//!   `recommend`, `assess`, `analyze legal`, `risk`,
//!   `production`, `irreversible` push toward Complex.
//! - **Imperative simplicity** — short prompts starting with
//!   `summarize`, `translate`, `format`, `extract`,
//!   `convert`, `define` go to Simple.
//!
//! Operators can override the classification per call by
//! tagging the request with an explicit tier hint (future
//! follow-up — not in this commit).

use serde::{Deserialize, Serialize};

/// One of the three tiers the §7.29 router supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningTier {
    /// Tier 1 — Simple. Cheapest fastest model.
    Simple,
    /// Tier 2 — Medium. Balanced model.
    Medium,
    /// Tier 3 — Complex. Strongest available model.
    Complex,
}

impl ReasoningTier {
    /// Stable string form used by config + audit lines.
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningTier::Simple => "simple",
            ReasoningTier::Medium => "medium",
            ReasoningTier::Complex => "complex",
        }
    }

    /// Match a `[reasoning.judge] apply_to` entry like
    /// `"tier3"` against this tier. Case-insensitive.
    pub fn matches_apply_to(self, tag: &str) -> bool {
        let t = tag.trim().to_ascii_lowercase();
        match self {
            ReasoningTier::Simple => matches!(t.as_str(), "tier1" | "simple"),
            ReasoningTier::Medium => matches!(t.as_str(), "tier2" | "medium"),
            ReasoningTier::Complex => matches!(t.as_str(), "tier3" | "complex"),
        }
    }
}

/// Rule-based classifier. Cheap to construct (no state); held
/// behind a static / cheap clone in the AI handler.
#[derive(Clone, Copy, Debug, Default)]
pub struct ComplexityClassifier;

impl ComplexityClassifier {
    pub fn new() -> Self {
        Self
    }

    /// Classify a prompt into a [`ReasoningTier`].
    ///
    /// `irreversible_hint = true` tells the classifier the
    /// dispatcher has already flagged the call as irreversible
    /// (e.g. the cap descriptor's reversibility flag), which
    /// promotes any non-Complex classification to Complex
    /// because Complex is where the judge model runs in the
    /// default config.
    pub fn classify(&self, prompt: &str, irreversible_hint: bool) -> ReasoningTier {
        let lower = prompt.trim().to_ascii_lowercase();
        if lower.is_empty() {
            return ReasoningTier::Simple;
        }

        let signals = score_signals(&lower);
        let mut tier = decide_tier(&signals, prompt.len());

        if irreversible_hint {
            tier = ReasoningTier::Complex;
        }
        tier
    }
}

/// Internal: the four structural signals. Exposed for tests
/// + diagnostic logging via `classify_with_signals`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Signals {
    /// Imperative-simple verbs at the start of the prompt
    /// (summarize, translate, format, …).
    pub simple_imperative: bool,
    /// Tokens like `decide`, `evaluate`, `recommend`, that
    /// indicate a judgement call.
    pub decision_marker_hits: u32,
    /// Tokens like `irreversible`, `production`, `payment`,
    /// `deploy`, `legal`, `risk`, `irrevocable`.
    pub high_stakes_hits: u32,
    /// Tokens like `function`, `algorithm`, `derive`, `prove`,
    /// `regression`, that indicate code/math reasoning.
    pub code_math_hits: u32,
}

fn score_signals(lower_prompt: &str) -> Signals {
    let simple_imperative = SIMPLE_IMPERATIVE_PREFIXES
        .iter()
        .any(|p| lower_prompt.starts_with(p));
    let decision_marker_hits = count_word_hits(lower_prompt, DECISION_MARKERS);
    let high_stakes_hits = count_word_hits(lower_prompt, HIGH_STAKES_MARKERS);
    let code_math_hits = count_word_hits(lower_prompt, CODE_MATH_MARKERS);
    Signals {
        simple_imperative,
        decision_marker_hits,
        high_stakes_hits,
        code_math_hits,
    }
}

fn decide_tier(s: &Signals, len: usize) -> ReasoningTier {
    // Order matters — higher tier short-circuits.
    if s.high_stakes_hits >= 1 || s.decision_marker_hits >= 2 {
        return ReasoningTier::Complex;
    }
    if s.code_math_hits >= 1 || s.decision_marker_hits == 1 || len > 600 {
        return ReasoningTier::Medium;
    }
    if s.simple_imperative && len <= 280 {
        return ReasoningTier::Simple;
    }
    if len <= 200 {
        ReasoningTier::Simple
    } else {
        ReasoningTier::Medium
    }
}

/// Whole-word substring scan that returns the number of
/// matched markers. Simple-but-fast scan; profiles to
/// micro-seconds on a 1KB prompt.
fn count_word_hits(haystack: &str, markers: &[&str]) -> u32 {
    let mut hits = 0u32;
    for m in markers {
        if contains_word(haystack, m) {
            hits += 1;
        }
    }
    hits
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    // Word-boundary substring: needle is surrounded by non-
    // alphanumeric chars (or start/end of string).
    let needle_lc = needle.to_ascii_lowercase();
    let bytes = haystack.as_bytes();
    let nlen = needle_lc.len();
    if nlen == 0 || bytes.len() < nlen {
        return false;
    }
    let mut i = 0;
    while i + nlen <= bytes.len() {
        if haystack[i..i + nlen].eq_ignore_ascii_case(&needle_lc) {
            let left_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_';
            let right_idx = i + nlen;
            let right_ok = right_idx == bytes.len()
                || !bytes[right_idx].is_ascii_alphanumeric() && bytes[right_idx] != b'_';
            if left_ok && right_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

const SIMPLE_IMPERATIVE_PREFIXES: &[&str] = &[
    "summarize ",
    "summarise ",
    "translate ",
    "format ",
    "extract ",
    "convert ",
    "define ",
    "list ",
    "rephrase ",
    "spell ",
    "capitalize ",
    "capitalise ",
    "lowercase ",
    "uppercase ",
    "title-case ",
];

const DECISION_MARKERS: &[&str] = &[
    "decide",
    "decision",
    "recommend",
    "recommendation",
    "evaluate",
    "evaluation",
    "assess",
    "compare",
    "trade-off",
    "tradeoff",
    "weigh",
    "judge",
    "verdict",
];

const HIGH_STAKES_MARKERS: &[&str] = &[
    "irreversible",
    "irrevocable",
    "production",
    "payment",
    "deploy",
    "deployment",
    "legal",
    "contract",
    "risk",
    "compliance",
    "regulatory",
    "incident",
    "outage",
    "money",
    "wire transfer",
    "live customer",
    "delete database",
    "drop table",
];

const CODE_MATH_MARKERS: &[&str] = &[
    "algorithm",
    "derive",
    "regression",
    "function",
    "prove",
    "lemma",
    "matrix",
    "vector",
    "differential",
    "integral",
    "stack trace",
    "exception",
    "compile error",
    "lint error",
    "type error",
    "debug",
    "refactor",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(p: &str) -> ReasoningTier {
        ComplexityClassifier::new().classify(p, false)
    }

    #[test]
    fn imperative_short_summarize_lands_simple() {
        assert_eq!(
            classify("summarize this email in two bullets"),
            ReasoningTier::Simple
        );
    }

    #[test]
    fn translate_short_lands_simple() {
        assert_eq!(
            classify("translate 'hello' to spanish"),
            ReasoningTier::Simple
        );
    }

    #[test]
    fn decision_marker_alone_lands_medium() {
        let p = "evaluate these three vendors and tell me which has the best support";
        assert_eq!(classify(p), ReasoningTier::Medium);
    }

    #[test]
    fn two_decision_markers_lands_complex() {
        let p = "decide and recommend whether to pivot the product line";
        assert_eq!(classify(p), ReasoningTier::Complex);
    }

    #[test]
    fn high_stakes_keyword_lands_complex_regardless_of_length() {
        let p = "is this contract risk acceptable?";
        assert_eq!(classify(p), ReasoningTier::Complex);
    }

    #[test]
    fn code_math_keyword_lands_medium() {
        let p = "debug this rust function that's leaking memory";
        assert_eq!(classify(p), ReasoningTier::Medium);
    }

    #[test]
    fn long_prompt_promotes_to_medium() {
        let long = "x".repeat(800);
        assert_eq!(classify(&long), ReasoningTier::Medium);
    }

    #[test]
    fn irreversible_hint_promotes_to_complex_always() {
        let c = ComplexityClassifier::new();
        let t = c.classify("summarize this email", true);
        assert_eq!(t, ReasoningTier::Complex);
    }

    #[test]
    fn empty_prompt_is_simple() {
        assert_eq!(classify(""), ReasoningTier::Simple);
        assert_eq!(classify("   "), ReasoningTier::Simple);
    }

    #[test]
    fn contains_word_respects_boundaries() {
        // "deployment" must not match "deploy" mid-word? Actually
        // "deploy" is a sub-word of "deployment", and "deployment"
        // is itself a high-stakes marker, so both should match. The
        // real boundary case: "redeploy" should still match "deploy"
        // because there's no non-alnum boundary on the left — verify
        // that we do NOT match a word-boundary case incorrectly.
        assert!(contains_word("we need to deploy now", "deploy"));
        assert!(!contains_word("nothing to redeploy", "deploy"));
        assert!(contains_word("deployment plan ready", "deployment"));
    }

    #[test]
    fn tier_as_str_round_trips_through_apply_to() {
        let t = ReasoningTier::Complex;
        assert_eq!(t.as_str(), "complex");
        assert!(t.matches_apply_to("tier3"));
        assert!(t.matches_apply_to("complex"));
        assert!(t.matches_apply_to(" COMPLEX "));
        assert!(!t.matches_apply_to("tier2"));
    }
}
