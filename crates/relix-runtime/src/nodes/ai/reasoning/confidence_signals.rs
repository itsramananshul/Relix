//! GAP 16 Component 2 — three-signal confidence aggregation.
//!
//! The §7.29 spec calls for three independent signals:
//!
//! 1. **Self-consistency** — ask the same question N times,
//!    cluster the answers, and score consistency by the
//!    largest cluster's share. 1 cluster = max confidence.
//! 2. **Retrieval quality** — vector similarity between the
//!    retrieved chunks and the answer. **Deferred follow-up**
//!    inside this commit: it needs per-call retrieval
//!    context the AI handler doesn't currently carry. The
//!    [`ThreeSignalConfidence`] aggregator accepts the
//!    signal as an `Option<f32>`; when absent the aggregator
//!    redistributes the remaining 65% across the other two
//!    signals proportionally so a deployment without
//!    retrieval still gets a meaningful score.
//! 3. **Judge scan** — a lightweight cheap-tier model checks
//!    the answer for obvious reasoning gaps. We use the same
//!    `crate::nodes::ai::reasoning::judge` plumbing (5
//!    questions, threshold-based verdict) but score it as a
//!    fraction of passed questions rather than a flag count.
//!
//! Spec weights: self_consistency 0.40, retrieval 0.35,
//! judge 0.25.
//!
//! Output bands (per spec):
//! - HIGH   > 0.85 → proceed automatically
//! - MEDIUM 0.60 – 0.85 → warn / log
//! - LOW    < 0.60 → pause / ask human

use serde::{Deserialize, Serialize};

/// Spec weights for the three-signal aggregation.
const W_SELF_CONSISTENCY: f32 = 0.40;
const W_RETRIEVAL_QUALITY: f32 = 0.35;
const W_JUDGE: f32 = 0.25;

/// Output of one self-consistency pass.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SelfConsistencyOutcome {
    /// Number of samples that landed in the same cluster as
    /// the modal answer. `cluster_size / total` is the
    /// per-spec self-consistency score.
    pub cluster_size: u32,
    /// Total samples drawn.
    pub total: u32,
    /// The canonical text of the modal answer (used by the
    /// caller to surface to the operator).
    pub modal_answer: String,
}

impl SelfConsistencyOutcome {
    /// Score in 0..=1.
    pub fn score(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.cluster_size as f32) / (self.total as f32)
        }
    }
}

/// Aggregator. Takes whichever signals are present and emits
/// a [`ThreeSignalScore`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreeSignalConfidence;

impl ThreeSignalConfidence {
    /// Combine the three signals into a 0..=1 score.
    ///
    /// `judge_passes_of_five` is the count of questions the
    /// judge marked `pass: true` (out of 5). When `None`,
    /// the judge signal is treated as not-present and its
    /// weight is redistributed across the available signals.
    ///
    /// `retrieval_quality` is in 0..=1; `None` skips the
    /// signal (the deferred follow-up case).
    ///
    /// `self_consistency` is the output of one
    /// [`SelfConsistencyOutcome::score`]; `None` skips it.
    pub fn aggregate(
        &self,
        self_consistency: Option<f32>,
        retrieval_quality: Option<f32>,
        judge_passes_of_five: Option<u32>,
    ) -> ThreeSignalScore {
        let judge_score = judge_passes_of_five.map(|p| (p.min(5) as f32) / 5.0);
        let signals = [
            (self_consistency, W_SELF_CONSISTENCY),
            (retrieval_quality, W_RETRIEVAL_QUALITY),
            (judge_score, W_JUDGE),
        ];
        let mut numerator = 0.0_f32;
        let mut denominator = 0.0_f32;
        for (value, weight) in &signals {
            if let Some(v) = *value {
                numerator += v.clamp(0.0, 1.0) * weight;
                denominator += weight;
            }
        }
        let score = if denominator > 0.0 {
            (numerator / denominator).clamp(0.0, 1.0)
        } else {
            0.0
        };
        ThreeSignalScore {
            score,
            band: classify_band(score),
            self_consistency,
            retrieval_quality,
            judge: judge_score,
        }
    }
}

/// Confidence band per spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfidenceBand {
    /// > 0.85 → proceed automatically.
    High,
    /// 0.60..=0.85 → proceed with warning logged.
    Medium,
    /// < 0.60 → pause / get more info / ask human.
    Low,
}

impl ConfidenceBand {
    pub fn as_str(self) -> &'static str {
        match self {
            ConfidenceBand::High => "high",
            ConfidenceBand::Medium => "medium",
            ConfidenceBand::Low => "low",
        }
    }
}

/// Aggregated score + per-signal breakdown.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreeSignalScore {
    pub score: f32,
    pub band: ConfidenceBand,
    pub self_consistency: Option<f32>,
    pub retrieval_quality: Option<f32>,
    pub judge: Option<f32>,
}

fn classify_band(score: f32) -> ConfidenceBand {
    if score > 0.85 {
        ConfidenceBand::High
    } else if score >= 0.60 {
        ConfidenceBand::Medium
    } else {
        ConfidenceBand::Low
    }
}

/// Cluster `samples` by exact-after-canonicalisation text
/// match and return the modal cluster as a
/// [`SelfConsistencyOutcome`].
///
/// Canonicalisation: trim, collapse whitespace, lowercase.
/// Production callers can swap in a semantic clusterer
/// (cosine similarity on embeddings) without changing this
/// module's API.
pub fn cluster_self_consistency_samples(samples: &[String]) -> SelfConsistencyOutcome {
    let total = samples.len() as u32;
    if total == 0 {
        return SelfConsistencyOutcome {
            cluster_size: 0,
            total: 0,
            modal_answer: String::new(),
        };
    }
    let mut counts: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut canonical_to_original: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for s in samples {
        let canon = canonicalise(s);
        *counts.entry(canon.clone()).or_insert(0) += 1;
        canonical_to_original
            .entry(canon)
            .or_insert_with(|| s.clone());
    }
    let (canon, cluster_size) = counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)))
        .unwrap_or_else(|| (String::new(), 0));
    let modal_answer = canonical_to_original.remove(&canon).unwrap_or_default();
    SelfConsistencyOutcome {
        cluster_size,
        total,
        modal_answer,
    }
}

fn canonicalise(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = true;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            for low in ch.to_lowercase() {
                out.push(low);
            }
            last_was_space = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn cluster_picks_the_modal_answer_from_close_variants() {
        let samples = s(&[
            "The deadline is Friday March 7th",
            "Based on the calendar, March 7th",
            "Friday the 7th of March",
            "the deadline is friday march 7th",
        ]);
        // Two exact-after-canon matches → modal cluster size 2,
        // total 4 → score 0.5.
        let o = cluster_self_consistency_samples(&samples);
        assert_eq!(o.total, 4);
        assert_eq!(o.cluster_size, 2);
        assert!((o.score() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn cluster_handles_empty_input() {
        let o = cluster_self_consistency_samples(&[]);
        assert_eq!(o.total, 0);
        assert_eq!(o.cluster_size, 0);
        assert!(o.modal_answer.is_empty());
        assert_eq!(o.score(), 0.0);
    }

    #[test]
    fn unanimous_samples_give_score_one() {
        let samples = s(&["yes", "yes", "yes"]);
        let o = cluster_self_consistency_samples(&samples);
        assert_eq!(o.cluster_size, 3);
        assert_eq!(o.score(), 1.0);
    }

    #[test]
    fn aggregate_with_all_three_signals_uses_spec_weights() {
        let agg = ThreeSignalConfidence;
        let r = agg.aggregate(Some(1.0), Some(1.0), Some(5));
        assert!((r.score - 1.0).abs() < 1e-6);
        assert_eq!(r.band, ConfidenceBand::High);
    }

    #[test]
    fn aggregate_redistributes_weight_when_retrieval_absent() {
        let agg = ThreeSignalConfidence;
        // self-consistency = 1.0, judge = 5/5 = 1.0, retrieval absent.
        // Score = (0.4 + 0.25) / (0.4 + 0.25) = 1.0.
        let r = agg.aggregate(Some(1.0), None, Some(5));
        assert!((r.score - 1.0).abs() < 1e-6);
        assert!(r.retrieval_quality.is_none());
    }

    #[test]
    fn aggregate_low_band_when_all_signals_low() {
        let agg = ThreeSignalConfidence;
        let r = agg.aggregate(Some(0.3), Some(0.3), Some(1));
        assert!(r.score < 0.6);
        assert_eq!(r.band, ConfidenceBand::Low);
    }

    #[test]
    fn aggregate_medium_band_falls_between_floors() {
        let agg = ThreeSignalConfidence;
        // Self-consistency 0.7, judge 4/5 = 0.8, retrieval 0.7.
        // Score = 0.4*0.7 + 0.35*0.7 + 0.25*0.8 = 0.725 → MEDIUM.
        let r = agg.aggregate(Some(0.7), Some(0.7), Some(4));
        assert!(r.score >= 0.60 && r.score <= 0.85);
        assert_eq!(r.band, ConfidenceBand::Medium);
    }

    #[test]
    fn aggregate_with_no_signals_returns_zero() {
        let agg = ThreeSignalConfidence;
        let r = agg.aggregate(None, None, None);
        assert_eq!(r.score, 0.0);
        assert_eq!(r.band, ConfidenceBand::Low);
    }

    #[test]
    fn band_thresholds_match_the_spec() {
        assert_eq!(classify_band(0.95), ConfidenceBand::High);
        assert_eq!(classify_band(0.85), ConfidenceBand::Medium);
        assert_eq!(classify_band(0.60), ConfidenceBand::Medium);
        assert_eq!(classify_band(0.59), ConfidenceBand::Low);
    }

    #[test]
    fn canonicalise_lowercases_and_collapses_whitespace() {
        assert_eq!(canonicalise("  Hello   WORLD  "), "hello world".to_string());
    }
}
