//! RELIX-7.19 — `ConfidenceScorer`.
//!
//! Pure-function scoring + a tiny rolling window per
//! `(agent, method)` pair. Designed for the dispatch hot
//! path: every operation is O(window) or O(1), no allocations
//! beyond the bounded `VecDeque`s, no SQLite, no syscalls.
//!
//! The five sub-scores are:
//!
//! - **`response_length`** — empty response scores 0.0
//!   (short-circuits the whole call); very-short responses
//!   (<10 estimated tokens) scale up linearly; 10–500 tokens
//!   score full marks; beyond 500 starts to taper.
//! - **`response_coherence`** — combines two §7.15 training
//!   QualityScorer heuristics: a "ends with proper sentence
//!   punctuation" bump and a "repeated-trigram ratio" penalty.
//! - **`provider_signal`** — a probability extracted from the
//!   response body when the provider emits `finish_reason` or
//!   `logprob`. `stop` → 1.0, `length` → 0.55, `content_filter`
//!   → 0.30, anything else → 0.50. `logprob` (when present) is
//!   mapped through `exp(logprob)` and clamped to [0,1]. Both
//!   present → average. Neither → neutral 0.50.
//! - **`error_rate_history`** — `1.0 - error_rate` from the
//!   rolling window. Empty window → 1.0 (no signal).
//! - **`latency_signal`** — `1.0` when `latency_ms <=
//!   baseline`; linear taper to `0.0` at `4 * baseline`.
//!
//! Final score is the weighted average. When
//! `error_rate >= 0.5` the final score is multiplied by
//! `error_rate_discount` (default 0.5) — a brittle
//! provider gets its scores halved across the board.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::config::{ConfidenceConfig, ConfidenceWeights};

/// Inputs the scorer needs to compute a verdict. The bridge
/// fills these from the handler outcome + per-call metadata.
#[derive(Clone, Debug, Default)]
pub struct ScoringInputs<'a> {
    /// Raw response body bytes (handler outcome `Ok` payload).
    /// Empty for failed calls.
    pub response_body: &'a [u8],
    /// Provider-reported finish reason — e.g. `"stop"`,
    /// `"length"`, `"content_filter"`. The dispatcher extracts
    /// this from the response when it's an AI cap.
    pub finish_reason: Option<&'a str>,
    /// Provider-reported per-token average log-probability.
    /// Maps to `exp(logprob)` clamped to [0, 1]. `None` when
    /// the provider didn't include it.
    pub logprob: Option<f32>,
    /// Handler-elapsed milliseconds.
    pub latency_ms: u64,
    /// True iff the handler returned Ok. Failures
    /// short-circuit `latency_signal` to 0.0 and bypass
    /// `response_length`/`coherence` (since there's no body).
    pub success: bool,
    /// RELIX-7.29 PART 2: self-consistency score. When
    /// `Some`, this value REPLACES the `provider_signal`
    /// sub-score before the weighted sum is computed. The AI
    /// handler emits the hint via
    /// [`crate::metrics::AiSelfConsistencyHint`]; the
    /// dispatch bridge reads it from the sink's join cache
    /// and threads it through here.
    pub self_consistency: Option<f32>,
}

/// Per-call scoring verdict.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceScore {
    /// The final weighted-average score, post `error_rate_discount`.
    /// Always in `[0.0, 1.0]`.
    pub final_score: f32,
    /// Each sub-score, exposed for diagnostics +
    /// `confidence.score_history`. Order:
    /// `response_length`, `response_coherence`, `provider_signal`,
    /// `error_rate_history`, `latency_signal`.
    pub sub_scores: SubScores,
    /// The rolling-window error rate (0.0 == no errors,
    /// 1.0 == all errors) at the time of scoring.
    pub rolling_error_rate: f32,
    /// True iff the `error_rate_discount` multiplier was
    /// applied to the final score. Helps operators see
    /// exactly when a brittle provider is dragging scores
    /// down.
    pub discount_applied: bool,
}

/// Per-criterion sub-score breakdown. Each value is in
/// `[0.0, 1.0]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SubScores {
    pub response_length: f32,
    pub response_coherence: f32,
    pub provider_signal: f32,
    pub error_rate_history: f32,
    pub latency_signal: f32,
}

/// Snapshot returned by `confidence.score_history`. The
/// p50/p95/p99 calculation reuses the
/// [`crate::metrics::query::percentile`] helper.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HistorySnapshot {
    pub agent: String,
    pub method: String,
    pub call_count: u64,
    pub error_count: u64,
    pub error_rate: f32,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
    pub p99_latency_ms: u64,
    pub avg_confidence: f32,
}

/// The scorer + its rolling-window state. Cheap to clone (one
/// `Arc<Mutex<HashMap>>`); the dispatch bridge holds it via
/// `Arc<ConfidenceScorer>` and serves the
/// `confidence.score_history` capability from the same
/// instance.
#[derive(Clone)]
pub struct ConfidenceScorer {
    weights: ConfidenceWeights,
    p95_baseline_ms: u64,
    window_size: usize,
    error_rate_discount: f32,
    /// Per-(agent, method) rolling state. `Mutex` is fine
    /// here — the critical section is microsecond-scale
    /// (push/pop on a VecDeque + a couple of integer
    /// updates) and contention is bounded to the dispatch
    /// hot path which is already serialised per request.
    state: std::sync::Arc<Mutex<HashMap<HistoryKey, WindowState>>>,
}

/// Composite key for the rolling-window HashMap.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct HistoryKey {
    agent: String,
    method: String,
}

/// Per-key rolling state. Two parallel VecDeques (one for
/// success/fail bits, one for latency ms) sized to
/// `window_size`; a third accumulator for confidence scores.
#[derive(Clone, Debug, Default)]
struct WindowState {
    outcomes: VecDeque<bool>,
    latencies: VecDeque<u64>,
    confidences: VecDeque<f32>,
}

impl WindowState {
    fn push(&mut self, success: bool, latency_ms: u64, confidence: f32, cap: usize) {
        if self.outcomes.len() == cap {
            self.outcomes.pop_front();
        }
        if self.latencies.len() == cap {
            self.latencies.pop_front();
        }
        if self.confidences.len() == cap {
            self.confidences.pop_front();
        }
        self.outcomes.push_back(success);
        self.latencies.push_back(latency_ms);
        self.confidences.push_back(confidence);
    }

    fn error_rate(&self) -> f32 {
        if self.outcomes.is_empty() {
            return 0.0;
        }
        let fails = self.outcomes.iter().filter(|s| !**s).count();
        fails as f32 / self.outcomes.len() as f32
    }
}

impl ConfidenceScorer {
    /// Build a scorer from a config block.
    pub fn from_config(cfg: &ConfidenceConfig) -> Self {
        Self {
            weights: cfg.weights,
            p95_baseline_ms: cfg.p95_latency_baseline_ms,
            window_size: cfg.window_size.max(1),
            error_rate_discount: cfg.error_rate_discount.clamp(0.0, 1.0),
            state: Default::default(),
        }
    }

    /// Compute a score WITHOUT mutating the rolling window.
    /// Used by tests + cases where the caller wants to peek
    /// the score before committing it (e.g. dry-run probes).
    pub fn score(&self, agent: &str, method: &str, inputs: &ScoringInputs<'_>) -> ConfidenceScore {
        // Empty + non-success short-circuits.
        if !inputs.success {
            // Failed calls are scored 0.0 — nothing else matters.
            return ConfidenceScore {
                final_score: 0.0,
                sub_scores: SubScores {
                    response_length: 0.0,
                    response_coherence: 0.0,
                    provider_signal: 0.0,
                    error_rate_history: 1.0 - self.peek_error_rate(agent, method),
                    latency_signal: 0.0,
                },
                rolling_error_rate: self.peek_error_rate(agent, method),
                discount_applied: false,
            };
        }
        if inputs.response_body.is_empty() {
            return ConfidenceScore {
                final_score: 0.0,
                sub_scores: SubScores {
                    response_length: 0.0,
                    response_coherence: 0.0,
                    provider_signal: 0.0,
                    error_rate_history: 1.0 - self.peek_error_rate(agent, method),
                    latency_signal: latency_score(inputs.latency_ms, self.p95_baseline_ms),
                },
                rolling_error_rate: self.peek_error_rate(agent, method),
                discount_applied: false,
            };
        }
        let text = std::str::from_utf8(inputs.response_body).unwrap_or("");
        let len_score = response_length_score(text);
        let coh_score = response_coherence_score(text);
        // RELIX-7.29 PART 2: when self-consistency sampling has
        // been run for this call, its score REPLACES the
        // `provider_signal` sub-score per spec — finish_reason /
        // logprob lose to the stronger N-sample agreement
        // signal whenever it's available.
        let prov_score = match inputs.self_consistency {
            Some(sc) => sc.clamp(0.0, 1.0),
            None => provider_signal_score(inputs.finish_reason, inputs.logprob),
        };
        let err_rate = self.peek_error_rate(agent, method);
        let err_score = 1.0 - err_rate;
        let lat_score = latency_score(inputs.latency_ms, self.p95_baseline_ms);

        let w = &self.weights;
        let total = w.total();
        let weighted = (len_score * w.response_length
            + coh_score * w.response_coherence
            + prov_score * w.provider_signal
            + err_score * w.error_rate_history
            + lat_score * w.latency_signal)
            / total;
        let mut final_score = weighted.clamp(0.0, 1.0);
        let discount_applied = if err_rate >= 0.5 {
            final_score *= self.error_rate_discount;
            true
        } else {
            false
        };
        ConfidenceScore {
            final_score: final_score.clamp(0.0, 1.0),
            sub_scores: SubScores {
                response_length: len_score,
                response_coherence: coh_score,
                provider_signal: prov_score,
                error_rate_history: err_score,
                latency_signal: lat_score,
            },
            rolling_error_rate: err_rate,
            discount_applied,
        }
    }

    /// Record one outcome in the rolling window. Idempotent
    /// with `score()` — the spec splits these so callers can
    /// score first (for the verdict) and then commit (after
    /// fallback may have decided to drop the call).
    pub fn record(
        &self,
        agent: &str,
        method: &str,
        success: bool,
        latency_ms: u64,
        confidence: f32,
    ) {
        let key = HistoryKey {
            agent: agent.to_string(),
            method: method.to_string(),
        };
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let st = g.entry(key).or_default();
        st.push(success, latency_ms, confidence, self.window_size);
    }

    /// Score + record in a single call — the production hot
    /// path uses this. Returns the verdict the caller passes
    /// to the [`super::FallbackEngine`].
    pub fn score_and_record(
        &self,
        agent: &str,
        method: &str,
        inputs: &ScoringInputs<'_>,
    ) -> ConfidenceScore {
        let verdict = self.score(agent, method, inputs);
        self.record(
            agent,
            method,
            inputs.success,
            inputs.latency_ms,
            verdict.final_score,
        );
        verdict
    }

    /// Read the rolling error rate for diagnostics without
    /// taking the write lock.
    pub fn peek_error_rate(&self, agent: &str, method: &str) -> f32 {
        let key = HistoryKey {
            agent: agent.to_string(),
            method: method.to_string(),
        };
        let g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        g.get(&key).map(|s| s.error_rate()).unwrap_or(0.0)
    }

    /// Returns a `HistorySnapshot` for one (agent, method)
    /// pair. Used by `confidence.score_history`. p50/p95/p99
    /// reuse [`crate::metrics::query::percentile`].
    pub fn snapshot(&self, agent: &str, method: &str) -> HistorySnapshot {
        let key = HistoryKey {
            agent: agent.to_string(),
            method: method.to_string(),
        };
        let g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(st) = g.get(&key) else {
            return HistorySnapshot {
                agent: agent.into(),
                method: method.into(),
                ..Default::default()
            };
        };
        let call_count = st.outcomes.len() as u64;
        let error_count = st.outcomes.iter().filter(|s| !**s).count() as u64;
        let error_rate = if call_count > 0 {
            error_count as f32 / call_count as f32
        } else {
            0.0
        };
        let mut latencies: Vec<u64> = st.latencies.iter().copied().collect();
        let p50 = crate::metrics::query::percentile(&mut latencies, 50.0);
        let p95 = crate::metrics::query::percentile(&mut latencies, 95.0);
        let p99 = crate::metrics::query::percentile(&mut latencies, 99.0);
        let avg_conf = if !st.confidences.is_empty() {
            st.confidences.iter().sum::<f32>() / st.confidences.len() as f32
        } else {
            0.0
        };
        HistorySnapshot {
            agent: agent.into(),
            method: method.into(),
            call_count,
            error_count,
            error_rate,
            p50_latency_ms: p50,
            p95_latency_ms: p95,
            p99_latency_ms: p99,
            avg_confidence: avg_conf,
        }
    }

    /// Reset every per-method window for `agent`. Returns the
    /// number of (agent, method) pairs that were cleared.
    pub fn reset_agent(&self, agent: &str) -> usize {
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let before = g.len();
        g.retain(|k, _| k.agent != agent);
        before - g.len()
    }

    /// Reset one (agent, method) window. Returns `true` if a
    /// window existed and was cleared.
    pub fn reset_pair(&self, agent: &str, method: &str) -> bool {
        let key = HistoryKey {
            agent: agent.to_string(),
            method: method.to_string(),
        };
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        g.remove(&key).is_some()
    }

    /// Test-only accessor for the window size — lets tests
    /// confirm the bound was honoured without poking the
    /// private struct.
    #[cfg(test)]
    pub(crate) fn window_size(&self) -> usize {
        self.window_size
    }
}

// ── sub-score helpers ──────────────────────────────────────

/// `0.0` for empty text; linear ramp from `0.3 → 1.0` over
/// `1..=10` estimated tokens; flat `1.0` over `10..=500`;
/// linear taper from `1.0 → 0.7` over `500..=2000`; flat `0.7`
/// beyond. "Estimated tokens" is `chars / 4`, the OpenAI
/// rule-of-thumb (matches the §7.15 training scorer).
fn response_length_score(text: &str) -> f32 {
    let chars = text.trim().chars().count();
    if chars == 0 {
        return 0.0;
    }
    let tokens = (chars as f32 / 4.0).max(1.0);
    if tokens < 10.0 {
        let frac = (tokens - 1.0) / 9.0; // 1→0.0, 10→1.0
        return 0.3 + 0.7 * frac.clamp(0.0, 1.0);
    }
    if tokens <= 500.0 {
        return 1.0;
    }
    if tokens <= 2000.0 {
        let frac = (tokens - 500.0) / 1500.0;
        return (1.0 - 0.3 * frac).clamp(0.7, 1.0);
    }
    0.7
}

/// Combines two heuristics:
///
/// - **End-with-punctuation bonus**: `1.0` when the trimmed
///   text ends with `.`, `?`, `!`, `."`, `…`. `0.7` otherwise.
/// - **Repeated-trigram penalty**: ratio of unique trigrams
///   to total trigrams. Below `0.5` repetition (i.e. lots of
///   duplicate trigrams) caps the score at `0.5`.
fn response_coherence_score(text: &str) -> f32 {
    let t = text.trim();
    if t.is_empty() {
        return 0.0;
    }
    let punct = matches!(
        t.chars().last(),
        Some('.') | Some('?') | Some('!') | Some('…') | Some('"') | Some(')') | Some(']')
    );
    let base = if punct { 1.0 } else { 0.7 };
    // Trigram uniqueness ratio.
    let words: Vec<&str> = t.split_whitespace().collect();
    if words.len() < 3 {
        return base; // not enough for a trigram check
    }
    let mut grams: std::collections::HashSet<(&str, &str, &str)> = std::collections::HashSet::new();
    let mut total = 0usize;
    for w in words.windows(3) {
        total += 1;
        grams.insert((w[0], w[1], w[2]));
    }
    let uniq_ratio = grams.len() as f32 / total as f32;
    if uniq_ratio < 0.5 {
        return base.min(0.5);
    }
    base
}

/// `finish_reason` → score, with a logprob override when
/// present. Both present → average.
fn provider_signal_score(finish_reason: Option<&str>, logprob: Option<f32>) -> f32 {
    let fr = finish_reason.map(|r| match r.to_ascii_lowercase().as_str() {
        "stop" => 1.0_f32,
        "length" => 0.55,
        "content_filter" => 0.30,
        _ => 0.50,
    });
    let lp = logprob.map(|v| v.exp().clamp(0.0, 1.0));
    match (fr, lp) {
        (Some(a), Some(b)) => (a + b) * 0.5,
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => 0.50,
    }
}

/// `1.0` when `latency_ms <= baseline`. Linear taper to `0.0`
/// at `4 * baseline`. Caps at `0.0`. A `baseline` of 0 is
/// treated as "no signal" → `1.0`.
fn latency_score(latency_ms: u64, baseline_ms: u64) -> f32 {
    if baseline_ms == 0 {
        return 1.0;
    }
    if latency_ms <= baseline_ms {
        return 1.0;
    }
    let max = (baseline_ms * 4) as f32;
    let over = (latency_ms.saturating_sub(baseline_ms)) as f32;
    let span = max - baseline_ms as f32;
    if span <= 0.0 {
        return 0.0;
    }
    (1.0 - over / span).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn s() -> ConfidenceScorer {
        ConfidenceScorer::from_config(&ConfidenceConfig {
            enabled: true,
            ..Default::default()
        })
    }

    #[test]
    fn an_empty_response_scores_zero() {
        let v = s().score(
            "a",
            "m",
            &ScoringInputs {
                response_body: b"",
                success: true,
                ..Default::default()
            },
        );
        assert!(v.final_score < 1e-6);
    }

    #[test]
    fn a_very_short_response_scores_lower_than_optimal_length() {
        let scr = s();
        let short = scr.score(
            "a",
            "m",
            &ScoringInputs {
                response_body: b"hi",
                success: true,
                ..Default::default()
            },
        );
        let optimal = scr.score(
            "a",
            "m",
            &ScoringInputs {
                response_body: b"This is a complete, well-formed answer that ends in a period.",
                success: true,
                ..Default::default()
            },
        );
        assert!(
            short.sub_scores.response_length < optimal.sub_scores.response_length,
            "short={:?} optimal={:?}",
            short.sub_scores,
            optimal.sub_scores
        );
    }

    #[test]
    fn a_response_ending_with_punctuation_scores_higher() {
        let scr = s();
        let bad = scr.score(
            "a",
            "m",
            &ScoringInputs {
                response_body: b"This is a longer answer without proper end",
                success: true,
                ..Default::default()
            },
        );
        let good = scr.score(
            "a",
            "m",
            &ScoringInputs {
                response_body: b"This is a longer answer without proper end.",
                success: true,
                ..Default::default()
            },
        );
        assert!(good.sub_scores.response_coherence > bad.sub_scores.response_coherence);
    }

    #[test]
    fn finish_reason_stop_scores_higher_than_length() {
        let scr = s();
        let body: &[u8] = b"reply text with enough chars to cross the length floor";
        let stop = scr.score(
            "a",
            "m",
            &ScoringInputs {
                response_body: body,
                finish_reason: Some("stop"),
                success: true,
                ..Default::default()
            },
        );
        let trunc = scr.score(
            "a",
            "m",
            &ScoringInputs {
                response_body: body,
                finish_reason: Some("length"),
                success: true,
                ..Default::default()
            },
        );
        assert!(stop.sub_scores.provider_signal > trunc.sub_scores.provider_signal);
    }

    #[test]
    fn a_high_error_rate_in_the_rolling_window_discounts_the_final_score() {
        let scr = s();
        // Seed the window with 6 failures + 4 successes → error_rate=0.6.
        for _ in 0..6 {
            scr.record("a", "m", false, 50, 0.0);
        }
        for _ in 0..4 {
            scr.record("a", "m", true, 50, 1.0);
        }
        let body = b"a perfectly clean, well-formed response that ends with a period.";
        let v = scr.score(
            "a",
            "m",
            &ScoringInputs {
                response_body: body,
                finish_reason: Some("stop"),
                latency_ms: 50,
                success: true,
                ..Default::default()
            },
        );
        assert!(
            v.discount_applied,
            "expected discount when error_rate >= 0.5, got {:?}",
            v
        );
        let clean_scr = s();
        let baseline = clean_scr.score(
            "a",
            "m",
            &ScoringInputs {
                response_body: body,
                finish_reason: Some("stop"),
                latency_ms: 50,
                success: true,
                ..Default::default()
            },
        );
        assert!(
            v.final_score < baseline.final_score,
            "discounted {} should be < baseline {}",
            v.final_score,
            baseline.final_score
        );
    }

    #[test]
    fn a_slow_response_scores_lower_than_a_fast_one() {
        let scr = s();
        let body = b"a complete, well-formed response that ends with a period.";
        let fast = scr.score(
            "a",
            "m",
            &ScoringInputs {
                response_body: body,
                latency_ms: 100,
                success: true,
                ..Default::default()
            },
        );
        let slow = scr.score(
            "a",
            "m",
            &ScoringInputs {
                response_body: body,
                latency_ms: 4500, // 3x the default baseline of 1500
                success: true,
                ..Default::default()
            },
        );
        assert!(slow.sub_scores.latency_signal < fast.sub_scores.latency_signal);
    }

    #[test]
    fn rolling_window_is_bounded_to_window_size() {
        let mut cfg = ConfidenceConfig {
            enabled: true,
            ..Default::default()
        };
        cfg.window_size = 5;
        let scr = ConfidenceScorer::from_config(&cfg);
        for i in 0..50 {
            scr.record("a", "m", i % 2 == 0, 100, 1.0);
        }
        // With size=5, exactly the most-recent 5 outcomes are
        // counted toward error_rate.
        assert_eq!(scr.window_size(), 5);
        let snap = scr.snapshot("a", "m");
        assert_eq!(snap.call_count, 5);
    }

    #[test]
    fn reset_history_clears_per_pair_state() {
        let scr = s();
        scr.record("a", "m", false, 10, 0.0);
        scr.record("a", "m", false, 10, 0.0);
        assert!(scr.peek_error_rate("a", "m") > 0.0);
        assert!(scr.reset_pair("a", "m"));
        assert_eq!(scr.peek_error_rate("a", "m"), 0.0);
    }

    #[test]
    fn reset_agent_clears_every_method_under_that_agent() {
        let scr = s();
        scr.record("a", "m1", false, 10, 0.0);
        scr.record("a", "m2", false, 10, 0.0);
        scr.record("b", "m1", false, 10, 0.0);
        let cleared = scr.reset_agent("a");
        assert_eq!(cleared, 2);
        assert_eq!(scr.peek_error_rate("a", "m1"), 0.0);
        assert_eq!(scr.peek_error_rate("a", "m2"), 0.0);
        assert!(scr.peek_error_rate("b", "m1") > 0.0);
    }

    #[test]
    fn snapshot_reports_p50_p95_p99_via_metrics_percentile() {
        let scr = s();
        for ms in [10u64, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
            scr.record("a", "m", true, ms, 1.0);
        }
        let snap = scr.snapshot("a", "m");
        assert_eq!(snap.call_count, 10);
        // Nearest-rank ties-broken-by-ceil: p50 of {10..100 step 10} = 50.
        assert_eq!(snap.p50_latency_ms, 50);
        assert!(snap.p95_latency_ms >= 90);
    }

    #[test]
    fn scorer_adds_under_1ms_latency_over_1000_responses() {
        // Spec acceptance: scorer must not add >1ms per call.
        // We measure 1000 calls and assert the AMORTISED per-
        // call cost is well under 1ms. (Total budget = 1s.)
        let scr = s();
        let body = b"a perfectly clean response with enough body to pass the length check.";
        let start = Instant::now();
        for i in 0..1000 {
            let v = scr.score_and_record(
                "agent",
                "ai.chat",
                &ScoringInputs {
                    response_body: body,
                    finish_reason: Some(if i % 5 == 0 { "length" } else { "stop" }),
                    latency_ms: 200 + (i as u64 % 100),
                    success: true,
                    ..Default::default()
                },
            );
            // Force the optimiser to keep the verdict.
            assert!(v.final_score >= 0.0);
        }
        let total = start.elapsed();
        let per_call_us = total.as_micros() / 1000;
        assert!(
            per_call_us < 1000,
            "scorer too slow: {per_call_us}us/call, total {:?}",
            total
        );
    }

    #[test]
    fn provider_signal_combines_finish_reason_and_logprob_when_both_present() {
        let v = provider_signal_score(Some("stop"), Some(-0.1f32)); // exp(-0.1) ~= 0.905
        // Average of 1.0 and ~0.905 = ~0.95.
        assert!(v > 0.9 && v <= 1.0);
    }

    #[test]
    fn failed_calls_score_zero_regardless_of_body() {
        let v = s().score(
            "a",
            "m",
            &ScoringInputs {
                response_body: b"some text",
                success: false,
                ..Default::default()
            },
        );
        assert_eq!(v.final_score, 0.0);
    }
}
