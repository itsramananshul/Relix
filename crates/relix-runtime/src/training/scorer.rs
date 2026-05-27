//! RELIX-7.15 — quality scorer.
//!
//! Assigns a `quality_score` in `[0.0, 1.0]` to a recorded
//! interaction using only deterministic, byte-level heuristics
//! (no ML, no provider calls). The score is the **product** of
//! five sub-scores so that a failure on any single dimension
//! reasonably penalises the overall record, and a perfect
//! interaction tops out at exactly `1.0`.
//!
//! Sub-scores:
//!
//! 1. **success** — `1.0` when the interaction completed
//!    successfully, `0.0` otherwise. Multiplicative, so a
//!    failed interaction always scores `0.0` regardless of the
//!    other dimensions.
//! 2. **response length** — uses approximate-token heuristic
//!    (1 token ≈ 4 chars). 50–500 tokens: `1.0`. Short
//!    (<20 tokens): `0.2`. Tail (>2000 tokens): `0.6`. Linear
//!    interpolation between bands.
//! 3. **latency** — under 2000ms: `1.0`; over 10000ms: `0.3`;
//!    linear interpolation between.
//! 4. **tool success rate** — `1.0` when no tool calls were
//!    made; otherwise (successful_tools / total_tools), with a
//!    floor of `0.0` when every tool call failed.
//! 5. **coherence** — `0.6` baseline + `+0.2` when the
//!    response ends with sentence-terminator punctuation +
//!    `+0.2` when no repeated tri-gram dominates the response.
//!
//! The background scorer loop pulls up to `batch_size`
//! unscored interactions every `interval_secs` seconds and
//! writes the computed score back to the store.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;

use super::store::TrainingStore;
use super::types::InteractionRecord;

/// One score computation result + the per-criterion sub-scores.
/// Exposed so operators can render "why was this scored this
/// way" in the UI.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScoreBreakdown {
    pub success: f32,
    pub response_length: f32,
    pub latency: f32,
    pub tool_success: f32,
    pub coherence: f32,
    pub total: f32,
}

impl ScoreBreakdown {
    pub fn zero() -> Self {
        Self {
            success: 0.0,
            response_length: 0.0,
            latency: 0.0,
            tool_success: 0.0,
            coherence: 0.0,
            total: 0.0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScorerConfig {
    pub interval: Duration,
    pub batch_size: u32,
}

impl Default for ScorerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
            batch_size: 50,
        }
    }
}

/// Stateless scorer — `score(record)` is pure.
#[derive(Clone, Copy, Debug, Default)]
pub struct QualityScorer;

impl QualityScorer {
    /// Compute the score breakdown for one record.
    pub fn breakdown(rec: &InteractionRecord) -> ScoreBreakdown {
        if !rec.success {
            return ScoreBreakdown::zero();
        }
        let response_length = score_response_length(&rec.response);
        let latency = score_latency(rec.latency_ms);
        let tool_success = score_tool_success(rec);
        let coherence = score_coherence(&rec.response);
        let success = 1.0;
        let total =
            (success * response_length * latency * tool_success * coherence).clamp(0.0, 1.0);
        ScoreBreakdown {
            success,
            response_length,
            latency,
            tool_success,
            coherence,
            total,
        }
    }

    /// Convenience wrapper returning only the final score.
    pub fn score(rec: &InteractionRecord) -> f32 {
        Self::breakdown(rec).total
    }
}

fn approx_tokens(s: &str) -> usize {
    // 1 token ≈ 4 chars per OpenAI rule of thumb. We use chars
    // (not bytes) so multi-byte UTF-8 doesn't inflate the count.
    s.chars().count() / 4
}

fn score_response_length(response: &str) -> f32 {
    let toks = approx_tokens(response) as f32;
    // Band shape: rises from 0.2 at 0 tokens to 1.0 at 50,
    // flat 1.0 between 50 and 500, decays to 0.6 by 2000+ tokens.
    if toks < 20.0 {
        // 0.2 at 0 → 0.6 at 20.
        0.2 + (toks / 20.0) * 0.4
    } else if toks < 50.0 {
        // 0.6 at 20 → 1.0 at 50.
        0.6 + ((toks - 20.0) / 30.0) * 0.4
    } else if toks <= 500.0 {
        1.0
    } else if toks <= 2000.0 {
        // 1.0 at 500 → 0.6 at 2000.
        1.0 - ((toks - 500.0) / 1500.0) * 0.4
    } else {
        0.6
    }
}

fn score_latency(ms: u64) -> f32 {
    let ms = ms as f32;
    if ms <= 2000.0 {
        1.0
    } else if ms >= 10_000.0 {
        0.3
    } else {
        // Linear interpolation 1.0 → 0.3 across 2000..10000ms.
        1.0 - ((ms - 2000.0) / 8000.0) * 0.7
    }
}

fn score_tool_success(rec: &InteractionRecord) -> f32 {
    if rec.tool_calls.is_empty() {
        return 1.0;
    }
    let total = rec.tool_calls.len() as f32;
    let ok = rec.tool_calls.iter().filter(|c| c.success).count() as f32;
    (ok / total).clamp(0.0, 1.0)
}

fn score_coherence(response: &str) -> f32 {
    if response.trim().is_empty() {
        return 0.0;
    }
    let mut score: f32 = 0.6;
    if ends_with_terminator(response) {
        score += 0.2;
    }
    if !has_dominant_repeat(response) {
        score += 0.2;
    }
    score.clamp(0.0, 1.0)
}

fn ends_with_terminator(response: &str) -> bool {
    let trimmed = response.trim_end();
    matches!(
        trimmed.chars().next_back(),
        Some('.') | Some('!') | Some('?') | Some('。') | Some(')')
    )
}

/// Detect a single tri-gram (whitespace-tokenised) that
/// dominates the response. A tri-gram is "dominant" when it
/// repeats `>= 3` times AND accounts for more than 25% of the
/// total tri-gram count — strong signal of a degenerate model
/// loop without flagging normal phrasing.
fn has_dominant_repeat(response: &str) -> bool {
    let words: Vec<&str> = response.split_whitespace().collect();
    if words.len() < 6 {
        return false;
    }
    use std::collections::HashMap;
    let mut counts: HashMap<(String, String, String), u32> = HashMap::new();
    for w in words.windows(3) {
        let key = (
            w[0].to_lowercase(),
            w[1].to_lowercase(),
            w[2].to_lowercase(),
        );
        *counts.entry(key).or_insert(0) += 1;
    }
    let total: u32 = counts.values().sum();
    if total == 0 {
        return false;
    }
    let max = counts.values().max().copied().unwrap_or(0);
    max >= 3 && (max as f32 / total as f32) > 0.25
}

/// Spawn the background scorer loop. Returns the JoinHandle so
/// callers can keep it alive (drop = stop). Production code
/// drops the handle.
pub fn spawn_scorer_loop(store: TrainingStore, cfg: ScorerConfig) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(cfg.interval);
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = score_one_batch(&store, cfg.batch_size) {
                tracing::warn!(error = %e, "training: scorer batch failed");
            }
        }
    })
}

/// Process one batch synchronously. Exposed for tests and for
/// the `training.score_interaction` capability which uses
/// `score_one(&store, id)` instead, but the batch path is
/// useful for "catch up after backfill" operator commands.
pub fn score_one_batch(
    store: &TrainingStore,
    batch_size: u32,
) -> Result<usize, super::store::TrainingStoreError> {
    let unscored = store.list_unscored(batch_size)?;
    let n = unscored.len();
    for rec in unscored {
        let score = QualityScorer::score(&rec);
        store.set_quality_score(rec.interaction_id.as_str(), Some(score))?;
    }
    Ok(n)
}

/// Score one interaction by id. Returns `Ok(None)` when no row
/// matches (operator should map this to 404).
pub fn score_one(
    store: &TrainingStore,
    interaction_id: &str,
) -> Result<Option<f32>, super::store::TrainingStoreError> {
    let Some(rec) = store.get(interaction_id)? else {
        return Ok(None);
    };
    let score = QualityScorer::score(&rec);
    store.set_quality_score(interaction_id, Some(score))?;
    Ok(Some(score))
}

/// Boxed alias for the background scorer task handle.
pub type ScorerJoinHandle = Arc<JoinHandle<()>>;

#[cfg(test)]
mod tests {
    use super::super::types::{InteractionId, InteractionRecord, ToolCallRecord};
    use super::*;

    fn rec_ok(response: &str, latency_ms: u64) -> InteractionRecord {
        InteractionRecord {
            interaction_id: InteractionId::new(),
            session_id: "s".into(),
            agent: "alice".into(),
            model: "gpt-4o-mini".into(),
            provider: "openai".into(),
            system_prompt: "sys".into(),
            user_message: "what is rust?".into(),
            response: response.into(),
            tool_calls: vec![],
            token_count: Some(100),
            prompt_tokens: Some(40),
            completion_tokens: Some(60),
            latency_ms,
            success: true,
            error_kind: None,
            recorded_at: 100,
            quality_score: None,
            exported: false,
            export_set: None,
            anonymized: false,
        }
    }

    #[test]
    fn failed_interaction_scores_zero() {
        let mut r = rec_ok(&"hi there. ".repeat(20), 100);
        r.success = false;
        r.error_kind = Some("RESPONDER_INTERNAL".into());
        let b = QualityScorer::breakdown(&r);
        assert_eq!(b.total, 0.0);
        assert_eq!(QualityScorer::score(&r), 0.0);
    }

    #[test]
    fn short_response_scores_lower_than_optimal_length() {
        let short = rec_ok("hi.", 100);
        let optimal = rec_ok(&"hello world. ".repeat(40), 100); // ~120 tokens
        assert!(QualityScorer::score(&short) < QualityScorer::score(&optimal));
    }

    #[test]
    fn slow_response_scores_lower_than_fast() {
        let fast = rec_ok(&"hello world. ".repeat(40), 100);
        let slow = rec_ok(&"hello world. ".repeat(40), 9_000);
        assert!(QualityScorer::score(&slow) < QualityScorer::score(&fast));
    }

    #[test]
    fn very_slow_response_is_capped_at_03_latency_factor() {
        let s = score_latency(20_000);
        assert!((s - 0.3).abs() < 1e-4);
    }

    #[test]
    fn repeated_phrases_score_lower_than_coherent_response() {
        // Same approximate length, but one repeats a tri-gram.
        let repeat = rec_ok(
            "yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes.",
            100,
        );
        let coherent = rec_ok(
            "Rust is a systems programming language focused on safety. It checks ownership at compile time. The borrow checker prevents data races. Many production systems run on Rust today.",
            100,
        );
        assert!(QualityScorer::score(&repeat) < QualityScorer::score(&coherent));
    }

    #[test]
    fn tool_success_full_when_all_succeed() {
        let mut r = rec_ok(&"hello world. ".repeat(40), 100);
        r.tool_calls = vec![
            ToolCallRecord {
                tool: "web_fetch".into(),
                input: "x".into(),
                output: "y".into(),
                success: true,
                latency_ms: 50,
                error_kind: None,
            },
            ToolCallRecord {
                tool: "web_fetch".into(),
                input: "x".into(),
                output: "y".into(),
                success: true,
                latency_ms: 50,
                error_kind: None,
            },
        ];
        let r2 = rec_ok(&"hello world. ".repeat(40), 100);
        let with_failure = {
            let mut x = r.clone();
            x.tool_calls[0].success = false;
            x
        };
        assert!(QualityScorer::score(&r) > QualityScorer::score(&with_failure));
        // Reference no-tool-call sample.
        assert_eq!(
            QualityScorer::breakdown(&r2).tool_success,
            QualityScorer::breakdown(&r).tool_success
        );
    }

    #[test]
    fn ends_with_terminator_boosts_coherence() {
        let yes = score_coherence("Hello, world. This sentence ends properly.");
        let no = score_coherence("Hello, world. This sentence ends improperly");
        assert!(yes > no);
    }

    #[test]
    fn empty_response_scores_zero_coherence() {
        assert_eq!(score_coherence(""), 0.0);
        assert_eq!(score_coherence("   \n\t  "), 0.0);
    }

    #[test]
    fn background_scorer_writes_back_quality_score() {
        // Synchronously drive `score_one_batch` rather than wait
        // on the loop's tokio timer.
        let store = TrainingStore::in_memory().unwrap();
        let r = rec_ok(&"hello world. ".repeat(40), 100);
        store.insert(&r).unwrap();
        let n = score_one_batch(&store, 10).unwrap();
        assert_eq!(n, 1);
        let got = store.get(r.interaction_id.as_str()).unwrap().unwrap();
        let s = got.quality_score.unwrap();
        assert!(s > 0.5);
    }

    #[test]
    fn score_one_returns_none_for_missing_id() {
        let store = TrainingStore::in_memory().unwrap();
        let r = score_one(&store, "ghost").unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn spawn_scorer_loop_processes_unscored_rows() {
        let store = TrainingStore::in_memory().unwrap();
        store
            .insert(&rec_ok(&"hello world. ".repeat(40), 100))
            .unwrap();
        let handle = spawn_scorer_loop(
            store.clone(),
            ScorerConfig {
                interval: Duration::from_millis(50),
                batch_size: 50,
            },
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        let unscored = store.list_unscored(10).unwrap();
        assert!(
            unscored.is_empty(),
            "background loop should have scored row"
        );
        handle.abort();
    }

    #[test]
    fn optimal_band_returns_full_length_score() {
        // 50 tokens (200 chars).
        let s = score_response_length(&"a".repeat(200));
        assert_eq!(s, 1.0);
        // 500 tokens (2000 chars).
        let s = score_response_length(&"a".repeat(2000));
        assert_eq!(s, 1.0);
    }

    #[test]
    fn very_long_response_decays_to_tail_score() {
        let s = score_response_length(&"a".repeat(20_000));
        assert!((s - 0.6).abs() < 1e-4);
    }
}
