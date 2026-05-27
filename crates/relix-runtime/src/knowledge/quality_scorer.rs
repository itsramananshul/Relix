//! RELIX-7.16 GAP 1 — MemoryQualityScorer.
//!
//! Background task that walks Layer 3 observation records
//! every `interval_secs` seconds and stamps a `quality:<f>`
//! tag on every observation that doesn't already carry one.
//! The [`crate::knowledge::trust::TrustChecker`] reads this
//! tag when enforcing the per-group `min_quality_score`
//! floor, so without this loop operators would have to stamp
//! the tag by hand.
//!
//! Scoring is deterministic + cheap — no provider calls, no
//! cross-process state. Mirrors the §7.15
//! [`crate::training::QualityScorer`] approach so operators
//! see the same shape of score across both surfaces, but the
//! input is a `MemoryRecord` (no latency / tool_calls / token
//! count) so the criteria are:
//!
//! - **Layer baseline** — observations score higher than raw
//!   by default (the promoter has already deduplicated +
//!   curated them). Configurable via
//!   `[knowledge.quality_scorer] observation_baseline`.
//! - **Text length** — text-length-as-token-proxy band:
//!   shortest snippets score low (they're rarely useful as
//!   shared knowledge); very long ones decay slightly so a
//!   wall-of-text doesn't dominate the share path.
//! - **Coherence** — punctuation-terminator bonus + dominant-
//!   tri-gram penalty (matches the training scorer's
//!   approach).
//!
//! Score is clamped into `[0.0, 1.0]` and stamped as a
//! `quality:<f>` tag with three decimal places of precision —
//! enough for operator min-quality floors like `0.700`.
//!
//! Idempotency: rows already carrying a `quality:` tag are
//! skipped — both at SQL-fetch time (the
//! [`crate::nodes::memory::schema::LayeredMemoryStore::fetch_unscored_observations`]
//! query filters them out) and at insert time (the
//! `add_tag` helper is a no-op on duplicates). Re-running the
//! task is safe.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

use crate::nodes::memory::schema::{LayeredMemoryStore, MemoryRecord};

/// `[knowledge.quality_scorer]` config block. Absent /
/// `enabled = false` leaves the task unspawned and existing
/// behaviour byte-identical.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryQualityScorerConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_batch_size")]
    pub batch_size: u32,
    /// Baseline score for a Layer 3 observation BEFORE the
    /// length + coherence modifiers run. Defaults to 0.75 so
    /// a clean observation lands at `0.75 * length_band *
    /// coherence` which is intentionally below the canonical
    /// 0.8 group floor — operators need a clean, coherent,
    /// optimally-sized observation to clear the share gate.
    #[serde(default = "default_observation_baseline")]
    pub observation_baseline: f32,
}

fn default_enabled() -> bool {
    true
}
fn default_interval_secs() -> u64 {
    60
}
fn default_batch_size() -> u32 {
    50
}
fn default_observation_baseline() -> f32 {
    0.75
}

impl Default for MemoryQualityScorerConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            interval_secs: default_interval_secs(),
            batch_size: default_batch_size(),
            observation_baseline: default_observation_baseline(),
        }
    }
}

/// One score result. Returned by
/// [`MemoryQualityScorer::score`] + by the background task's
/// per-row pass so tests can inspect the formula breakdown.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScoreBreakdown {
    pub baseline: f32,
    pub length: f32,
    pub coherence: f32,
    pub total: f32,
}

/// Pure scorer — takes a config + a [`MemoryRecord`], returns
/// a 0.0..1.0 score.
#[derive(Clone, Copy, Debug, Default)]
pub struct MemoryQualityScorer;

impl MemoryQualityScorer {
    /// Compute the breakdown for `rec` under `cfg`.
    pub fn breakdown(rec: &MemoryRecord, cfg: &MemoryQualityScorerConfig) -> ScoreBreakdown {
        let baseline = cfg.observation_baseline.clamp(0.0, 1.0);
        let length = score_length(&rec.text);
        let coherence = score_coherence(&rec.text);
        let total = (baseline * length * coherence).clamp(0.0, 1.0);
        ScoreBreakdown {
            baseline,
            length,
            coherence,
            total,
        }
    }

    /// Final score for `rec` under `cfg`. Convenience wrapper
    /// over [`Self::breakdown`].
    pub fn score(rec: &MemoryRecord, cfg: &MemoryQualityScorerConfig) -> f32 {
        Self::breakdown(rec, cfg).total
    }
}

fn approx_tokens(s: &str) -> usize {
    s.chars().count() / 4
}

fn score_length(text: &str) -> f32 {
    let toks = approx_tokens(text) as f32;
    // Band shape tuned for Layer 3 observations: short facts
    // (< 5 tokens) score lower because they're often
    // headlines / single words. The optimal band is 10..200
    // tokens — typical observation length. Very long
    // observations (> 500 tokens) decay slightly: they're
    // usually un-promoted summaries the curator hasn't tidied
    // yet.
    if toks < 5.0 {
        // 0.3 at 0 → 0.7 at 5.
        0.3 + (toks / 5.0) * 0.4
    } else if toks < 10.0 {
        // 0.7 at 5 → 1.0 at 10.
        0.7 + ((toks - 5.0) / 5.0) * 0.3
    } else if toks <= 200.0 {
        1.0
    } else if toks <= 500.0 {
        // 1.0 at 200 → 0.7 at 500.
        1.0 - ((toks - 200.0) / 300.0) * 0.3
    } else {
        0.7
    }
}

fn score_coherence(text: &str) -> f32 {
    if text.trim().is_empty() {
        return 0.0;
    }
    let mut score: f32 = 0.6;
    if ends_with_terminator(text) {
        score += 0.2;
    }
    if !has_dominant_repeat(text) {
        score += 0.2;
    }
    score.clamp(0.0, 1.0)
}

fn ends_with_terminator(text: &str) -> bool {
    let trimmed = text.trim_end();
    matches!(
        trimmed.chars().next_back(),
        Some('.') | Some('!') | Some('?') | Some('。') | Some(')')
    )
}

fn has_dominant_repeat(text: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
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

/// Format a score as a stable `quality:<f>` tag with three
/// decimal places. Exposed so the test surface + the dispatch
/// glue both agree on the wire format.
pub fn format_quality_tag(score: f32) -> String {
    let clamped = score.clamp(0.0, 1.0);
    format!("quality:{clamped:.3}")
}

/// Process one batch synchronously. Exposed for tests + for
/// the spawn-loop. Returns the number of records scored.
pub fn score_one_batch(
    store: &LayeredMemoryStore,
    cfg: &MemoryQualityScorerConfig,
) -> Result<usize, crate::nodes::memory::schema::LayeredMemoryError> {
    let batch = store.fetch_unscored_observations(cfg.batch_size)?;
    let mut n = 0usize;
    for rec in batch {
        // Belt-and-braces: the SQL fetch already filtered out
        // rows that carry a `quality:` tag, but if a record
        // shows up with the substring inside a non-tag column
        // (extraordinarily rare) we re-check the tag list
        // before stamping.
        if rec.tags.iter().any(|t| t.starts_with("quality:")) {
            continue;
        }
        let score = MemoryQualityScorer::score(&rec, cfg);
        store.add_tag(&rec.id, &format_quality_tag(score))?;
        n += 1;
    }
    Ok(n)
}

/// Spawn the background loop. Returns the `JoinHandle` so the
/// controller can keep it alive for the process lifetime
/// (production drops the handle; shutdown happens when
/// tokio's runtime tears down).
pub fn spawn_memory_quality_scorer(
    store: Arc<LayeredMemoryStore>,
    cfg: MemoryQualityScorerConfig,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let interval = Duration::from_secs(cfg.interval_secs.max(5));
        let mut tick = tokio::time::interval(interval);
        // Skip the immediate t=0 tick — operators expect the
        // first run to land at `interval_secs` after boot.
        tick.tick().await;
        loop {
            tick.tick().await;
            match score_one_batch(&store, &cfg) {
                Ok(0) => {
                    tracing::debug!("knowledge.quality_scorer: no unscored observations this tick");
                }
                Ok(n) => {
                    tracing::info!(
                        scored = n,
                        baseline = cfg.observation_baseline,
                        "knowledge.quality_scorer: stamped quality tags"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, "knowledge.quality_scorer: batch failed");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::memory::schema::{LayeredMemoryStore, MemoryLayer, MemoryRecord};

    fn cfg() -> MemoryQualityScorerConfig {
        MemoryQualityScorerConfig::default()
    }

    fn observation(id: &str, owner: &str, text: &str) -> MemoryRecord {
        let mut r = MemoryRecord::new_raw(id, text, owner);
        r.layer = MemoryLayer::Observation;
        r
    }

    #[test]
    fn unscored_observation_gets_a_quality_tag_on_next_tick() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        store
            .insert(&observation(
                "o1",
                "alice",
                "user prefers Helvetica for body text and Inter for headings.",
            ))
            .unwrap();
        let n = score_one_batch(&store, &cfg()).unwrap();
        assert_eq!(n, 1);
        let got = store.get("o1").unwrap().unwrap();
        let quality_tag = got
            .tags
            .iter()
            .find(|t| t.starts_with("quality:"))
            .expect("quality tag stamped");
        // Format is `quality:0.123`.
        let value: f32 = quality_tag
            .strip_prefix("quality:")
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            (0.0..=1.0).contains(&value),
            "quality must be in [0, 1]: {value}"
        );
    }

    #[test]
    fn record_already_carrying_a_quality_tag_is_not_re_scored() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        let mut r = observation("o1", "alice", "Layer 3 observation text body.");
        r.tags.push("quality:0.999".into());
        store.insert(&r).unwrap();
        let n = score_one_batch(&store, &cfg()).unwrap();
        assert_eq!(n, 0, "scored row count must be zero on a pre-tagged row");
        let got = store.get("o1").unwrap().unwrap();
        let qual_count = got
            .tags
            .iter()
            .filter(|t| t.starts_with("quality:"))
            .count();
        assert_eq!(qual_count, 1, "must not double-stamp");
        let tag = got.tags.iter().find(|t| t.starts_with("quality:")).unwrap();
        assert_eq!(tag, "quality:0.999", "operator tag is preserved verbatim");
    }

    #[test]
    fn quality_tag_format_is_three_decimal_places() {
        assert_eq!(format_quality_tag(0.0), "quality:0.000");
        assert_eq!(format_quality_tag(1.0), "quality:1.000");
        assert_eq!(format_quality_tag(0.123_456), "quality:0.123");
        // Above 1.0 clamps.
        assert_eq!(format_quality_tag(99.9), "quality:1.000");
        // Below 0 clamps.
        assert_eq!(format_quality_tag(-0.5), "quality:0.000");
    }

    #[test]
    fn repeated_trigrams_score_lower_than_coherent_text() {
        let cfg = cfg();
        let coherent = observation(
            "a",
            "alice",
            "Rust is a systems language. It checks ownership at compile time. \
             The borrow checker prevents data races at the type system level.",
        );
        let repeats = observation(
            "b",
            "alice",
            "yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes yes.",
        );
        let s_a = MemoryQualityScorer::score(&coherent, &cfg);
        let s_b = MemoryQualityScorer::score(&repeats, &cfg);
        assert!(s_a > s_b, "coherent must outscore repeats: {s_a} vs {s_b}");
    }

    #[test]
    fn observation_ending_with_punctuation_scores_higher() {
        let cfg = cfg();
        let with_dot = observation(
            "a",
            "alice",
            "user prefers concise summaries with citations.",
        );
        let no_dot = observation(
            "b",
            "alice",
            "user prefers concise summaries with citations",
        );
        assert!(
            MemoryQualityScorer::score(&with_dot, &cfg) > MemoryQualityScorer::score(&no_dot, &cfg)
        );
    }

    #[test]
    fn batch_size_limit_is_respected() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        for i in 0..10 {
            store
                .insert(&observation(
                    &format!("o{i}"),
                    "alice",
                    &format!("observation number {i} that has enough text to score."),
                ))
                .unwrap();
        }
        let mut cfg = cfg();
        cfg.batch_size = 3;
        let n = score_one_batch(&store, &cfg).unwrap();
        assert_eq!(n, 3, "batch_size cap honoured");
        // Run again: 3 more get stamped.
        let n2 = score_one_batch(&store, &cfg).unwrap();
        assert_eq!(n2, 3);
        // Total of 6 rows now have a quality tag.
        let mut tagged = 0;
        for i in 0..10 {
            let got = store.get(&format!("o{i}")).unwrap().unwrap();
            if got.tags.iter().any(|t| t.starts_with("quality:")) {
                tagged += 1;
            }
        }
        assert_eq!(tagged, 6);
    }

    #[test]
    fn fetch_unscored_only_returns_layer_3_observations() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        let mut raw = MemoryRecord::new_raw("r1", "raw line", "alice");
        raw.layer = MemoryLayer::Raw;
        store.insert(&raw).unwrap();
        let mut sem = MemoryRecord::new_raw("s1", "semantic line", "alice");
        sem.layer = MemoryLayer::Semantic;
        store.insert(&sem).unwrap();
        store
            .insert(&observation("o1", "alice", "observation text."))
            .unwrap();
        let unscored = store.fetch_unscored_observations(10).unwrap();
        assert_eq!(unscored.len(), 1);
        assert_eq!(unscored[0].id, "o1");
    }

    #[test]
    fn fetch_unscored_excludes_already_scored_rows() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        let mut r = observation("o1", "alice", "obs body");
        r.tags.push("quality:0.5".into());
        store.insert(&r).unwrap();
        store
            .insert(&observation("o2", "alice", "another obs body"))
            .unwrap();
        let unscored = store.fetch_unscored_observations(10).unwrap();
        assert_eq!(unscored.len(), 1);
        assert_eq!(unscored[0].id, "o2");
    }

    #[test]
    fn unscored_observations_returned_in_observed_at_ascending_order() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        for (id, ts) in [("c", 300), ("a", 100), ("b", 200)] {
            let mut r = observation(id, "alice", "obs body");
            r.observed_at = ts;
            store.insert(&r).unwrap();
        }
        let rows = store.fetch_unscored_observations(10).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn config_parses_minimal_toml() {
        let cfg: MemoryQualityScorerConfig = toml::from_str(
            r#"
            enabled = true
            interval_secs = 30
            batch_size = 100
            observation_baseline = 0.6
            "#,
        )
        .unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.interval_secs, 30);
        assert_eq!(cfg.batch_size, 100);
        assert!((cfg.observation_baseline - 0.6).abs() < 1e-4);
    }

    #[test]
    fn config_uses_documented_defaults_when_section_minimal() {
        let cfg: MemoryQualityScorerConfig = toml::from_str("enabled = true").unwrap();
        assert_eq!(cfg.interval_secs, 60);
        assert_eq!(cfg.batch_size, 50);
        assert!((cfg.observation_baseline - 0.75).abs() < 1e-4);
    }

    #[tokio::test]
    async fn background_loop_writes_quality_tag_within_one_tick() {
        let store = Arc::new(LayeredMemoryStore::in_memory().unwrap());
        store
            .insert(&observation(
                "o1",
                "alice",
                "Layer 3 observation body text.",
            ))
            .unwrap();
        let cfg = MemoryQualityScorerConfig {
            enabled: true,
            interval_secs: 5,
            batch_size: 50,
            observation_baseline: 0.75,
        };
        let handle = spawn_memory_quality_scorer(store.clone(), cfg);
        // Drive the tokio clock past the 5-second tick.
        tokio::time::pause();
        // First yield to let the task install its interval.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        // Resume so the task actually runs.
        tokio::time::resume();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let got = store.get("o1").unwrap().unwrap();
        let tagged = got.tags.iter().any(|t| t.starts_with("quality:"));
        assert!(
            tagged,
            "background loop must stamp a quality tag: tags = {:?}",
            got.tags
        );
        handle.abort();
    }
}
