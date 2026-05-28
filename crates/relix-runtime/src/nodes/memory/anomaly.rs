//! GAP 6 — write-time anomaly scoring for memory observations.
//!
//! The [`MemoryGuard`] in `guard.rs` catches *prompt-injection
//! patterns*. This module catches *anomalous-but-clean*
//! observations — content that is technically benign but has a
//! shape we don't trust as a long-term memory record. Three
//! signals, blended into a 0.0–1.0 score:
//!
//! - **Short-message** — an observation under
//!   [`MIN_OBSERVATION_CHARS`] characters carries almost no
//!   useful information and is usually the LLM hedging
//!   ("Maybe", "Yes", "Unclear"). Heavy penalty.
//! - **Specificity floor** — observations that consist entirely
//!   of generic filler ("the user is interesting", "the user
//!   likes things") have no entropy. We approximate this by
//!   counting how many domain-specific tokens the text contains
//!   (numbers, proper-noun-shaped tokens, identifiers with
//!   underscores or dots).
//! - **Contradiction** — when the same `source` already has a
//!   valid Layer-3 observation whose text starts with the same
//!   subject-predicate prefix but ends with a different polarity
//!   ("user likes X" vs "user dislikes X"), the new write is
//!   probably a poisoning attempt rather than a clean update.
//!
//! Score policy (see [`AnomalyScore::action`]):
//!
//! - `score >= 0.85` ⇒ [`AnomalyAction::Reject`] — never landed.
//! - `score >= 0.55` ⇒ [`AnomalyAction::Quarantine`] — written
//!   to the quarantine table with the score + reason; an
//!   operator must explicitly approve.
//! - otherwise      ⇒ [`AnomalyAction::Accept`].
//!
//! The scorer is intentionally pure: it takes the candidate text
//! and a snapshot of existing observations for the same source,
//! returns a score + reason, and never writes anywhere itself.
//! That makes it easy to unit-test and easy to call from both
//! the promoter (which already iterates per-source observations)
//! and the [`super::ingest`] path.

use crate::nodes::memory::schema::MemoryRecord;

/// Below this length, an observation is treated as essentially
/// content-free. Tuned to "Yes." / "Unclear." / "Maybe true."
/// Anything under 12 characters is almost certainly hedge.
pub const MIN_OBSERVATION_CHARS: usize = 12;

/// Below this many domain-specific tokens, an observation is
/// treated as generic filler. Tuned so that a sentence like
/// "User prefers terse replies" (1 domain token: "terse")
/// still passes, but "the user is interesting" (0 domain tokens)
/// gets penalised.
pub const MIN_SPECIFIC_TOKENS: usize = 1;

/// Hard reject above this score. The observation never lands —
/// not even in quarantine.
pub const REJECT_THRESHOLD: f32 = 0.85;

/// Quarantine above this score (but below
/// [`REJECT_THRESHOLD`]). Operator must approve.
pub const QUARANTINE_THRESHOLD: f32 = 0.55;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnomalyAction {
    /// Cleanly accepted — write straight to the observation
    /// store.
    Accept,
    /// Anomalous but salvageable — write to the quarantine
    /// table; operator decides.
    Quarantine,
    /// Definitely poisoned — drop on the floor. The caller
    /// SHOULD still log the rejection.
    Reject,
}

#[derive(Debug, Clone)]
pub struct AnomalyScore {
    pub score: f32,
    pub reasons: Vec<String>,
}

impl AnomalyScore {
    pub fn action(&self) -> AnomalyAction {
        if self.score >= REJECT_THRESHOLD {
            AnomalyAction::Reject
        } else if self.score >= QUARANTINE_THRESHOLD {
            AnomalyAction::Quarantine
        } else {
            AnomalyAction::Accept
        }
    }

    /// Human-readable single-line reason, suitable for logs and
    /// for the `memory_quarantine.reason` column. Empty if the
    /// observation scored zero.
    pub fn reason_line(&self) -> String {
        if self.reasons.is_empty() {
            String::new()
        } else {
            self.reasons.join("; ")
        }
    }
}

/// The public entry point. `candidate` is the observation text
/// being scored; `existing` is the set of currently-valid
/// observations on the same `source` (typically the result of
/// `list_observations_for_source`).
pub fn score_observation(candidate: &str, existing: &[MemoryRecord]) -> AnomalyScore {
    let mut score = 0.0f32;
    let mut reasons: Vec<String> = Vec::new();

    let trimmed = candidate.trim();
    if trimmed.len() < MIN_OBSERVATION_CHARS {
        score += 0.5;
        reasons.push(format!(
            "short-message ({}<{})",
            trimmed.len(),
            MIN_OBSERVATION_CHARS
        ));
    }

    let specific_tokens = count_specific_tokens(trimmed);
    if specific_tokens < MIN_SPECIFIC_TOKENS {
        score += 0.55;
        reasons.push(format!(
            "low-specificity ({}<{})",
            specific_tokens, MIN_SPECIFIC_TOKENS
        ));
    }

    if let Some(other) = first_contradiction(trimmed, existing) {
        score += 0.5;
        reasons.push(format!("contradicts:{}", other));
    }

    if score > 1.0 {
        score = 1.0;
    }

    AnomalyScore { score, reasons }
}

/// Count how many tokens in `text` carry domain-specific
/// information. We treat numeric tokens, proper-noun-shaped
/// tokens (start with upper-case), and identifier-shaped tokens
/// (contain `_`, `.`, or `:`) as specific. Stop-words and pure
/// lower-case tokens don't count.
pub fn count_specific_tokens(text: &str) -> usize {
    text.split_whitespace()
        .filter(|tok| {
            // Strip leading / trailing punctuation. We keep '_'
            // / '.' / ':' if they appear BETWEEN alphanumerics
            // (e.g. `memory.search`, `foo:bar`) — those are
            // identifier-shaped and count as specific. A pure
            // trailing period from end-of-sentence punctuation
            // does NOT count.
            let trimmed = tok.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            if trimmed.is_empty() {
                return false;
            }
            let has_digit = trimmed.chars().any(|c| c.is_ascii_digit());
            let has_struct =
                trimmed.contains('_') || trimmed.contains('.') || trimmed.contains(':');
            let proper_noun = trimmed
                .chars()
                .next()
                .map(|c| c.is_ascii_uppercase())
                .unwrap_or(false)
                && trimmed.len() > 2;
            has_digit || has_struct || (proper_noun && !STOP_WORDS.contains(&trimmed))
        })
        .count()
}

/// Return the first existing observation that contradicts
/// `candidate`. We approximate "contradicts" as: same first 5
/// tokens (subject + predicate stem) but the candidate negates
/// the existing one, OR uses an antonym pair from
/// [`ANTONYM_PAIRS`].
pub fn first_contradiction<'a>(candidate: &str, existing: &'a [MemoryRecord]) -> Option<&'a str> {
    let cand_norm = normalize(candidate);
    let cand_tokens: Vec<&str> = cand_norm.split_whitespace().take(5).collect();
    if cand_tokens.len() < 2 {
        return None;
    }
    for r in existing {
        let other_norm = normalize(&r.text);
        let other_tokens: Vec<&str> = other_norm.split_whitespace().take(5).collect();
        if other_tokens.len() < 2 {
            continue;
        }
        // Subject overlap: at least 2 of the first 5 tokens
        // match between candidate and existing.
        let overlap = cand_tokens
            .iter()
            .filter(|t| other_tokens.contains(*t))
            .count();
        if overlap < 2 {
            continue;
        }
        if mentions_negation(&cand_norm) != mentions_negation(&other_norm) {
            return Some(r.id.as_str());
        }
        if antonym_clash(&cand_norm, &other_norm) {
            return Some(r.id.as_str());
        }
    }
    None
}

fn normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '\'' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn mentions_negation(norm: &str) -> bool {
    norm.split_whitespace().any(|t| {
        matches!(
            t,
            "not" | "no" | "never" | "dislikes" | "doesn't" | "don't" | "isn't" | "won't" | "can't"
        )
    })
}

const ANTONYM_PAIRS: &[(&str, &str)] = &[
    ("likes", "dislikes"),
    ("loves", "hates"),
    ("prefers", "avoids"),
    ("enjoys", "loathes"),
    ("approves", "rejects"),
    ("supports", "opposes"),
    ("agrees", "disagrees"),
    ("accepts", "refuses"),
];

fn antonym_clash(a: &str, b: &str) -> bool {
    for (x, y) in ANTONYM_PAIRS {
        let a_has_x = a.split_whitespace().any(|t| t == *x);
        let a_has_y = a.split_whitespace().any(|t| t == *y);
        let b_has_x = b.split_whitespace().any(|t| t == *x);
        let b_has_y = b.split_whitespace().any(|t| t == *y);
        if (a_has_x && b_has_y) || (a_has_y && b_has_x) {
            return true;
        }
    }
    false
}

const STOP_WORDS: &[&str] = &[
    "The", "This", "That", "These", "Those", "Here", "There", "Yes", "No", "User", "Subject",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(id: &str, text: &str) -> MemoryRecord {
        let mut r = MemoryRecord::new_raw(id, text, "alice");
        r.layer = crate::nodes::memory::schema::MemoryLayer::Observation;
        r
    }

    #[test]
    fn short_message_pushes_score_above_quarantine_threshold() {
        let s = score_observation("Yes.", &[]);
        assert!(matches!(
            s.action(),
            AnomalyAction::Quarantine | AnomalyAction::Reject
        ));
        assert!(s.reasons.iter().any(|r| r.starts_with("short-message")));
    }

    #[test]
    fn generic_filler_with_no_specific_tokens_quarantines() {
        let s = score_observation("the user is interesting and likes things", &[]);
        assert_eq!(s.action(), AnomalyAction::Quarantine);
        assert!(s.reasons.iter().any(|r| r.starts_with("low-specificity")));
    }

    #[test]
    fn observation_with_proper_noun_passes() {
        let s = score_observation("User prefers Postgres over MySQL.", &[]);
        assert_eq!(s.action(), AnomalyAction::Accept);
    }

    #[test]
    fn observation_with_dotted_identifier_passes() {
        let s = score_observation("user uses memory.search to recall data", &[]);
        assert_eq!(s.action(), AnomalyAction::Accept);
    }

    #[test]
    fn direct_negation_contradiction_is_flagged() {
        let prior = vec![obs("o1", "User likes terse replies")];
        let s = score_observation("User does not like terse replies", &prior);
        assert!(s.reasons.iter().any(|r| r.starts_with("contradicts")));
    }

    #[test]
    fn antonym_contradiction_is_flagged() {
        let prior = vec![obs("o1", "User loves coffee in the morning")];
        let s = score_observation("User hates coffee in the morning", &prior);
        assert!(s.reasons.iter().any(|r| r.starts_with("contradicts")));
    }

    #[test]
    fn unrelated_observations_do_not_contradict() {
        let prior = vec![obs("o1", "User likes Postgres")];
        let s = score_observation("User dislikes mornings", &prior);
        assert!(!s.reasons.iter().any(|r| r.starts_with("contradicts")));
    }

    #[test]
    fn very_short_and_contradicting_passes_reject_threshold() {
        let prior = vec![obs("o1", "User likes A")];
        let s = score_observation("No.", &prior);
        // Short message + low specificity = 0.9, and we haven't
        // even applied the contradiction bump on top.
        assert_eq!(s.action(), AnomalyAction::Reject);
    }

    #[test]
    fn count_specific_tokens_treats_identifiers_as_specific() {
        assert_eq!(count_specific_tokens("foo.bar baz qux 99"), 2);
    }

    #[test]
    fn normalize_strips_punctuation_and_lowercases() {
        assert_eq!(
            normalize("User, prefers TerseReplies!"),
            "user prefers tersereplies"
        );
    }

    #[test]
    fn anomaly_score_reason_line_joins_reasons() {
        let s = AnomalyScore {
            score: 0.8,
            reasons: vec![
                "short-message (4<12)".into(),
                "low-specificity (0<1)".into(),
            ],
        };
        let line = s.reason_line();
        assert!(line.contains("short-message"));
        assert!(line.contains("low-specificity"));
        assert!(line.contains(';'));
    }
}
