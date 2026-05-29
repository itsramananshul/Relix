//! §7.29 Component 1 — request complexity classifier.
//!
//! Pure-function rule-based classifier that scores an incoming
//! request against seven structural signals and assigns it one
//! of three [`ComplexityTier`] buckets. The classifier is
//! deliberately NOT an LLM call — putting an LLM on every
//! hot-path classification would defeat the cost-saving
//! purpose of the smart router.
//!
//! ## Scoring signals (per spec)
//!
//! | Signal                                                    | Points |
//! |-----------------------------------------------------------|-------:|
//! | Message length: <50 words                                 |     +0 |
//! | Message length: 50–200 words                              |     +1 |
//! | Message length: >200 words                                |     +2 |
//! | Contains code blocks (``` markers)                        |     +1 |
//! | Contains multi-step markers (`1.`, `2.`, `first`, …)      |     +1 |
//! | Contains technical keyword (algorithm / refactor / …)     |     +1 |
//! | Contains explicit complexity marker (`think carefully`, …)|     +2 |
//! | Session has more than 5 prior turns                       |     +1 |
//! | Multi-topic heuristic (>3 distinct noun-phrase candidates)|     +1 |
//!
//! ## Tier mapping
//!
//! - 0–1 → [`ComplexityTier::Simple`]
//! - 2–3 → [`ComplexityTier::Medium`]
//! - 4+ → [`ComplexityTier::Complex`]
//!
//! The classifier is a pure function: takes the request text +
//! the session turn count, returns the score + the list of
//! signals that fired. No I/O, no state.

use serde::{Deserialize, Serialize};

/// One of the three §7.29 complexity tiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComplexityTier {
    /// Tier 1 — cheapest fastest model.
    Simple,
    /// Tier 2 — balanced model.
    Medium,
    /// Tier 3 — most capable model.
    Complex,
}

impl ComplexityTier {
    /// Stable lowercase string used in audit / metrics / wire
    /// formats.
    pub fn as_str(self) -> &'static str {
        match self {
            ComplexityTier::Simple => "simple",
            ComplexityTier::Medium => "medium",
            ComplexityTier::Complex => "complex",
        }
    }

    /// Parse the wire-format string back into a tier. Returns
    /// `None` for unknown strings; callers decide the default.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "simple" | "tier1" | "1" => Some(ComplexityTier::Simple),
            "medium" | "tier2" | "2" => Some(ComplexityTier::Medium),
            "complex" | "tier3" | "3" => Some(ComplexityTier::Complex),
            _ => None,
        }
    }

    /// One step "up" — the next more-capable tier. Used by the
    /// router's health fallback path (Simple→Medium→Complex).
    /// Complex stays Complex (terminal).
    pub fn next_up(self) -> Self {
        match self {
            ComplexityTier::Simple => ComplexityTier::Medium,
            ComplexityTier::Medium => ComplexityTier::Complex,
            ComplexityTier::Complex => ComplexityTier::Complex,
        }
    }
}

/// Output of one classifier run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComplexityScore {
    /// Resolved tier per the score → tier mapping.
    pub tier: ComplexityTier,
    /// Raw numeric score (sum of point contributions).
    pub score: i32,
    /// Names of the signals that fired, in evaluation order.
    /// Useful for operator diagnostics (`routing.explain`).
    pub signals_triggered: Vec<String>,
}

/// The classifier. Holds no state — every call is independent.
#[derive(Clone, Copy, Debug, Default)]
pub struct ComplexityClassifier;

impl ComplexityClassifier {
    /// Construct. Cheap; no allocation.
    pub fn new() -> Self {
        Self
    }

    /// Classify a request.
    ///
    /// `message` is the user's prompt verbatim. `session_turns`
    /// is the number of prior turns in the session (used by the
    /// "deep context" signal).
    pub fn classify(&self, message: &str, session_turns: u32) -> ComplexityScore {
        let mut signals: Vec<String> = Vec::new();
        let mut score = 0_i32;

        // ── 1. Message length ─────────────────────────────
        let words = count_words(message);
        if words > 200 {
            score += 2;
            signals.push("length>200_words".to_string());
        } else if words >= 50 {
            score += 1;
            signals.push("length_50_to_200_words".to_string());
        }

        // ── 2. Code blocks (``` markers) ──────────────────
        if has_code_block(message) {
            score += 1;
            signals.push("contains_code_block".to_string());
        }

        // ── 3. Multi-step instructions ────────────────────
        if has_multi_step_marker(message) {
            score += 1;
            signals.push("multi_step_instruction".to_string());
        }

        // ── 4. Technical keywords ─────────────────────────
        if let Some(kw) = find_technical_keyword(message) {
            score += 1;
            signals.push(format!("technical_keyword:{kw}"));
        }

        // ── 5. Explicit complexity marker ─────────────────
        if let Some(marker) = find_explicit_marker(message) {
            score += 2;
            signals.push(format!("explicit_marker:{marker}"));
        }

        // ── 6. Deep session context ───────────────────────
        if session_turns > 5 {
            score += 1;
            signals.push(format!("session_turns>{}", 5));
        }

        // ── 7. Multi-topic heuristic ──────────────────────
        if distinct_noun_phrases(message) > 3 {
            score += 1;
            signals.push("multi_topic".to_string());
        }

        let tier = match score {
            i if i <= 1 => ComplexityTier::Simple,
            i if i <= 3 => ComplexityTier::Medium,
            _ => ComplexityTier::Complex,
        };

        ComplexityScore {
            tier,
            score,
            signals_triggered: signals,
        }
    }
}

// ── helpers ──────────────────────────────────────────────

fn count_words(s: &str) -> usize {
    s.split_whitespace().filter(|w| !w.is_empty()).count()
}

fn has_code_block(s: &str) -> bool {
    s.contains("```")
}

/// Detect numbered or first/then/finally style multi-step
/// instructions. We look for at least TWO ordered markers so a
/// single `1.` in normal prose doesn't trip the signal.
fn has_multi_step_marker(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();

    // Numbered list: at least two of `1.`, `2.`, `3.`, `4.`
    // appearing at the start of any line OR after a newline.
    let numbered_hits = ["1.", "2.", "3.", "4."]
        .iter()
        .filter(|m| count_line_starts(s, m) > 0)
        .count();
    if numbered_hits >= 2 {
        return true;
    }

    // Sequential narrative: at least two of "first", "then",
    // "next", "finally" as standalone words.
    let narrative_hits = ["first", "then", "next", "finally", "afterwards"]
        .iter()
        .filter(|w| contains_word(&lower, w))
        .count();
    narrative_hits >= 2
}

fn count_line_starts(s: &str, prefix: &str) -> usize {
    let mut hits = 0;
    for line in s.lines() {
        if line.trim_start().starts_with(prefix) {
            hits += 1;
        }
    }
    hits
}

const TECHNICAL_KEYWORDS: &[&str] = &[
    "algorithm",
    "algorithms",
    "optimize",
    "optimise",
    "architecture",
    "architectural",
    "design",
    "implement",
    "implementation",
    "refactor",
    "refactored",
    "analyze",
    "analyse",
    "compare",
    "evaluate",
];

fn find_technical_keyword(s: &str) -> Option<&'static str> {
    let lower = s.to_ascii_lowercase();
    TECHNICAL_KEYWORDS
        .iter()
        .copied()
        .find(|kw| contains_word(&lower, kw))
}

const EXPLICIT_MARKERS: &[&str] = &[
    "think carefully",
    "step by step",
    "step-by-step",
    "reason through",
    "reason about",
    "walk through",
    "be thorough",
];

fn find_explicit_marker(s: &str) -> Option<&'static str> {
    let lower = s.to_ascii_lowercase();
    EXPLICIT_MARKERS.iter().copied().find(|m| lower.contains(m))
}

/// Whole-word substring check: needle must be flanked by
/// non-alphanumeric characters (or start/end of string).
fn contains_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let n = needle.len();
    if n == 0 || bytes.len() < n {
        return false;
    }
    let mut i = 0;
    while i + n <= bytes.len() {
        if haystack[i..i + n].eq_ignore_ascii_case(needle) {
            let left_ok = i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_');
            let right_idx = i + n;
            let right_ok = right_idx == bytes.len()
                || (!bytes[right_idx].is_ascii_alphanumeric() && bytes[right_idx] != b'_');
            if left_ok && right_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Rough noun-phrase counter — capitalised tokens (proper
/// nouns) plus content words after determiners (the X, a Y).
/// Deliberately a heuristic — heavy NLP is out of scope.
fn distinct_noun_phrases(s: &str) -> usize {
    // Strip code blocks so embedded identifiers don't inflate
    // the count.
    let cleaned = strip_code_blocks(s);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let determiners = ["the", "a", "an", "this", "that", "these", "those"];

    let lowered = cleaned.to_ascii_lowercase();
    let tokens: Vec<&str> = lowered
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|t| !t.is_empty())
        .collect();

    // Heuristic A: capitalised tokens in the original text
    // (anything starting with an uppercase letter that isn't a
    // sentence-initial common word).
    let mut sentence_initial = true;
    for raw in cleaned.split(|c: char| c.is_whitespace()) {
        if raw.is_empty() {
            continue;
        }
        let trimmed: String = raw.trim_matches(|c: char| !c.is_alphanumeric()).to_string();
        if trimmed.is_empty() {
            continue;
        }
        let first = trimmed.chars().next().unwrap();
        if first.is_ascii_uppercase() && !sentence_initial {
            seen.insert(trimmed.to_ascii_lowercase());
        }
        sentence_initial = raw.ends_with('.') || raw.ends_with('?') || raw.ends_with('!');
    }

    // Heuristic B: determiner + content word pairs.
    for (idx, tok) in tokens.iter().enumerate() {
        if determiners.contains(tok)
            && let Some(next) = tokens.get(idx + 1)
            && next.len() > 2
            && !determiners.contains(next)
        {
            seen.insert((*next).to_string());
        }
    }
    seen.len()
}

fn strip_code_blocks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_block = false;
    for line in s.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_block = !in_block;
            continue;
        }
        if !in_block {
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_simple_message_classifies_as_simple() {
        let c = ComplexityClassifier::new();
        let score = c.classify("what's the weather?", 0);
        assert_eq!(score.tier, ComplexityTier::Simple, "{score:?}");
        assert!(score.score <= 1);
    }

    #[test]
    fn explicit_think_carefully_marker_promotes_to_medium_or_complex() {
        let c = ComplexityClassifier::new();
        let score = c.classify("think carefully about this", 0);
        // +2 from marker → tier = medium.
        assert!(
            score
                .signals_triggered
                .iter()
                .any(|s| s.contains("explicit_marker")),
            "{score:?}"
        );
        assert!(score.tier == ComplexityTier::Medium || score.tier == ComplexityTier::Complex);
    }

    #[test]
    fn a_request_containing_a_code_block_triggers_the_code_signal() {
        let c = ComplexityClassifier::new();
        let msg = "Please review:\n```rust\nfn main() {}\n```\nThanks";
        let score = c.classify(msg, 0);
        assert!(
            score
                .signals_triggered
                .iter()
                .any(|s| s == "contains_code_block"),
            "{score:?}"
        );
    }

    #[test]
    fn long_multi_step_technical_message_classifies_as_complex() {
        let c = ComplexityClassifier::new();
        let body: String = (0..220).map(|i| format!("word{i} ")).collect();
        let msg =
            format!("1. first analyze the architecture.\n2. then refactor the algorithm.\n{body}");
        let score = c.classify(&msg, 6);
        assert_eq!(score.tier, ComplexityTier::Complex, "{score:?}");
        // Length>200 (+2) + multi-step (+1) + technical keyword (+1) + deep session (+1) ≥ 4.
        assert!(score.score >= 4, "{score:?}");
    }

    #[test]
    fn multi_step_markers_require_at_least_two_hits() {
        let c = ComplexityClassifier::new();
        // Single "1." in body should NOT trip the signal.
        let one_marker = c.classify("Step 1. say hi", 0);
        assert!(
            !one_marker
                .signals_triggered
                .iter()
                .any(|s| s == "multi_step_instruction"),
            "{one_marker:?}"
        );
    }

    #[test]
    fn session_turns_above_five_adds_a_signal() {
        let c = ComplexityClassifier::new();
        let none = c.classify("hi", 5);
        let some = c.classify("hi", 6);
        assert!(some.score >= none.score);
        assert!(
            some.signals_triggered
                .iter()
                .any(|s| s.starts_with("session_turns"))
        );
    }

    #[test]
    fn tier_string_round_trips() {
        for t in [
            ComplexityTier::Simple,
            ComplexityTier::Medium,
            ComplexityTier::Complex,
        ] {
            assert_eq!(ComplexityTier::parse(t.as_str()), Some(t));
        }
        assert_eq!(
            ComplexityTier::parse("tier3"),
            Some(ComplexityTier::Complex)
        );
        assert!(ComplexityTier::parse("nope").is_none());
    }

    #[test]
    fn next_up_walks_simple_medium_complex_and_terminates() {
        assert_eq!(ComplexityTier::Simple.next_up(), ComplexityTier::Medium);
        assert_eq!(ComplexityTier::Medium.next_up(), ComplexityTier::Complex);
        assert_eq!(ComplexityTier::Complex.next_up(), ComplexityTier::Complex);
    }

    #[test]
    fn distinct_noun_phrases_picks_up_capitalised_proper_nouns() {
        let n = distinct_noun_phrases(
            "I'm building a SaaS in Rust on AWS using the OpenAI API for chat.",
        );
        // SaaS / Rust / AWS / OpenAI / API → at least 3.
        assert!(n >= 3, "got {n}");
    }

    #[test]
    fn length_50_to_200_words_gives_one_point() {
        let c = ComplexityClassifier::new();
        let body = "hello there ".repeat(40); // ~80 words
        let score = c.classify(&body, 0);
        assert!(
            score
                .signals_triggered
                .iter()
                .any(|s| s == "length_50_to_200_words")
        );
    }

    #[test]
    fn length_over_200_words_gives_two_points() {
        let c = ComplexityClassifier::new();
        let body = "hello there ".repeat(120); // ~240 words
        let score = c.classify(&body, 0);
        assert!(
            score
                .signals_triggered
                .iter()
                .any(|s| s == "length>200_words")
        );
    }

    #[test]
    fn empty_message_classifies_as_simple_with_no_signals() {
        let c = ComplexityClassifier::new();
        let score = c.classify("", 0);
        assert_eq!(score.tier, ComplexityTier::Simple);
        assert!(score.signals_triggered.is_empty());
    }
}
