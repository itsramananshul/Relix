//! RELIX-7.29 PART 2 — Self-Consistency Sampling.
//!
//! Implements the §7.29 Component 2 spec piece that REPLACES
//! the `provider_signal` sub-score on the
//! [`super::scorer::ConfidenceScorer`] when active. The AI
//! handler:
//!
//! 1. Runs the baseline `ai.chat` as usual.
//! 2. If `[confidence.self_consistency]` is enabled AND the
//!    capability matches `capability_patterns` AND the
//!    baseline confidence dropped below `min_score_to_enable`,
//!    fires `sample_count` *parallel* `ai.chat` retries via
//!    `tokio::join_all` (same prompt, same model, default
//!    temperature spread).
//! 3. For each sample, [`extract_core_answer`] strips the
//!    preamble and trims to the first ~100 words. Each core
//!    answer is embedded with the existing
//!    `ChatProvider::generate_embedding`.
//! 4. Pairwise cosine similarities are averaged into the
//!    `self_consistency_score`. The highest-coherence sample
//!    (the one with the greatest average cosine to the
//!    others) becomes the final response body.
//! 5. The score is attached to the metrics sink as an
//!    [`crate::metrics::AiSelfConsistencyHint`] keyed by
//!    `request_id`. The dispatch bridge's
//!    [`crate::confidence::score_outcome`] equivalent reads
//!    the hint via the sink's join cache and substitutes the
//!    score for `provider_signal` before applying weights.
//!
//! Every operation in this module is a pure function — the AI
//! handler is responsible for sequencing the actual provider
//! calls.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::metrics::alert::{ActiveAlert, AlertDeliver, AlertEvent, AlertKind, AlertSeverity};
use crate::metrics::pricing::PriceTable;

/// `[confidence.self_consistency]` configuration block.
///
/// ```toml
/// [confidence.self_consistency]
/// enabled = true
/// sample_count = 3
/// min_score_to_enable = 0.70
/// capability_patterns = ["ai.chat", "ai.chat.*"]
/// ```
///
/// `min_score_to_enable` is the *adaptive* trigger: the AI
/// handler computes a cheap baseline confidence first; if it
/// is ≥ `min_score_to_enable`, the call ships with no SC
/// overhead. Below the threshold, the handler pays for the
/// N parallel samples + embedding pairwise comparison.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct SelfConsistencyConfig {
    /// Master switch. `false` (the default) keeps the AI
    /// handler byte-identical to its pre-SC behaviour.
    #[serde(default)]
    pub enabled: bool,
    /// How many parallel samples to dispatch. Spec default is
    /// 3. The handler rejects 0/1 as a no-op (you can't compute
    /// a pairwise cosine with fewer than two samples).
    #[serde(default = "default_sample_count")]
    pub sample_count: usize,
    /// Adaptive trigger threshold. When the baseline
    /// confidence is ≥ this value, no extra samples fire.
    /// Default 0.70.
    #[serde(default = "default_min_score_to_enable")]
    pub min_score_to_enable: f32,
    /// Capability matcher globs (same syntax as
    /// `[confidence.policies]` — literal, prefix `*`, suffix
    /// `*`, or surrounded `*`). Empty list means "match every
    /// capability".
    #[serde(default)]
    pub capability_patterns: Vec<String>,
    /// PART 3: SC trigger-rate guard. When the rolling 1000-
    /// sample trigger rate exceeds this percentage, the
    /// guard disables SC for `disable_duration_secs` and
    /// emits a [`super::AlertKind::CostAlert`] via the wired
    /// `MultiChannelAlertSink`. Default 50%.
    #[serde(default = "default_max_trigger_rate_pct")]
    pub max_trigger_rate_pct: u8,
    /// PART 3: how long SC stays disabled after a guard trip
    /// (trigger-rate or hourly-budget). Default 300s.
    #[serde(default = "default_disable_duration_secs")]
    pub disable_duration_secs: u64,
    /// PART 3: hourly absolute spend cap (USD) for SC samples.
    /// Crossing it fires a CostAlert and disables SC for
    /// `disable_duration_secs`. Default $10/hour.
    #[serde(default = "default_sc_hourly_budget_usd")]
    pub sc_hourly_budget_usd: f64,
    /// PART 3: per-request budget (USD). When the estimated
    /// cost of the optional stages (SC + judge + belief) for
    /// a single request exceeds this, the AI handler skips
    /// SC → judge → belief in that order. Default $1.
    #[serde(default = "default_per_request_budget_usd")]
    pub per_request_budget_usd: f64,
}

impl Default for SelfConsistencyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sample_count: default_sample_count(),
            min_score_to_enable: default_min_score_to_enable(),
            capability_patterns: Vec::new(),
            max_trigger_rate_pct: default_max_trigger_rate_pct(),
            disable_duration_secs: default_disable_duration_secs(),
            sc_hourly_budget_usd: default_sc_hourly_budget_usd(),
            per_request_budget_usd: default_per_request_budget_usd(),
        }
    }
}

fn default_sample_count() -> usize {
    3
}

fn default_min_score_to_enable() -> f32 {
    0.70
}

fn default_max_trigger_rate_pct() -> u8 {
    50
}

fn default_disable_duration_secs() -> u64 {
    300
}

fn default_sc_hourly_budget_usd() -> f64 {
    10.0
}

fn default_per_request_budget_usd() -> f64 {
    1.0
}

impl SelfConsistencyConfig {
    /// `true` when the config is active AND `cap` matches at
    /// least one configured pattern (or the pattern list is
    /// empty, treated as "everything").
    pub fn matches_capability(&self, cap: &str) -> bool {
        if !self.enabled || self.sample_count < 2 {
            return false;
        }
        if self.capability_patterns.is_empty() {
            return true;
        }
        self.capability_patterns
            .iter()
            .any(|p| super::fallback::glob_match(p, cap))
    }

    /// `true` when the baseline confidence is *below* the
    /// adaptive trigger threshold — i.e. the handler SHOULD pay
    /// for SC.
    pub fn should_trigger(&self, baseline_confidence: f32) -> bool {
        baseline_confidence < self.min_score_to_enable
    }
}

/// One sample's evaluation row. Used internally by the scorer
/// + returned via the `confidence.self_consistency_stats` cap.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SampleEvaluation {
    /// Original 0-based index of the sample.
    pub index: usize,
    /// Extracted core answer (first ~100 words, preamble
    /// stripped) used as the embedding source.
    pub core_answer: String,
    /// Average pairwise cosine similarity of this sample
    /// against every other sample.
    pub coherence: f32,
}

/// Aggregate verdict from N samples. Returned by
/// [`evaluate_samples`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SelfConsistencyOutcome {
    /// Average pairwise cosine across every (i, j) pair — this
    /// is the score that replaces `provider_signal`.
    pub score: f32,
    /// Index of the highest-coherence sample. The AI handler
    /// returns this sample's full body to the caller per the
    /// "returns highest-coherence sample" spec line.
    pub best_index: usize,
    /// Per-sample breakdown — useful for the
    /// `confidence.self_consistency_stats` cap + dashboards.
    pub samples: Vec<SampleEvaluation>,
}

impl Default for SelfConsistencyOutcome {
    fn default() -> Self {
        Self {
            score: 0.0,
            best_index: 0,
            samples: Vec::new(),
        }
    }
}

/// PART 3 cost-guard size for the rolling trigger ring.
pub(crate) const TRIGGER_RING_CAPACITY: usize = 1000;

/// PART 3 internal cost-guard state. Behind one Mutex so the
/// rolling structures stay consistent under the AI handler's
/// concurrent calls.
#[derive(Default)]
struct CostGuardState {
    /// Rolling decision ring — `true` ⇒ SC fired on that
    /// request, `false` ⇒ SC was considered but skipped (e.g.
    /// baseline confidence was high enough). Cap = 1000.
    trigger_ring: VecDeque<bool>,
    /// Rolling hourly SC spend events: `(unix_secs, usd)`.
    /// Prune-on-write keeps the deque bounded by elapsed time.
    hourly_spend_events: VecDeque<(i64, f64)>,
}

/// Process-wide rolling counters surfaced by the
/// `confidence.self_consistency_stats` cap.
#[derive(Clone, Default)]
pub struct SelfConsistencyStats {
    trigger_count: Arc<AtomicU64>,
    total_samples: Arc<AtomicU64>,
    score_sum_bits: Arc<AtomicU64>,
    score_count: Arc<AtomicU32>,
    last_score_bits: Arc<AtomicU32>,
    /// PART 3: unix-seconds the SC gate is disabled until.
    /// `i64::MIN` ⇒ never disabled.
    disabled_until_unix_secs: Arc<AtomicI64>,
    /// PART 3: cost-guard rolling state.
    guard: Arc<Mutex<CostGuardState>>,
    /// PART 3: alert sink for CostAlert emissions. Installed
    /// once at controller startup via
    /// [`Self::install_alert_sink`]. When absent, the guard
    /// still disables SC but skips the alert emission.
    alert_sink: Arc<OnceLock<Arc<dyn AlertDeliver>>>,
    /// PART 3: price table used by the per-request cost
    /// estimator. Installed once via
    /// [`Self::install_price_table`]. Without a price table
    /// the per-request budget gate is inert (returns `None`).
    price_table: Arc<OnceLock<Arc<PriceTable>>>,
}

impl SelfConsistencyStats {
    pub fn new() -> Self {
        Self {
            trigger_count: Arc::new(AtomicU64::new(0)),
            total_samples: Arc::new(AtomicU64::new(0)),
            score_sum_bits: Arc::new(AtomicU64::new(0)),
            score_count: Arc::new(AtomicU32::new(0)),
            last_score_bits: Arc::new(AtomicU32::new(0)),
            disabled_until_unix_secs: Arc::new(AtomicI64::new(i64::MIN)),
            guard: Arc::new(Mutex::new(CostGuardState::default())),
            alert_sink: Arc::new(OnceLock::new()),
            price_table: Arc::new(OnceLock::new()),
        }
    }

    /// PART 3: install the alert sink the cost guards emit
    /// CostAlerts through. Idempotent — second + later calls
    /// are silently ignored. Without a sink, the guards still
    /// disable SC; they just don't notify operators.
    pub fn install_alert_sink(&self, sink: Arc<dyn AlertDeliver>) {
        let _ = self.alert_sink.set(sink);
    }

    /// PART 3: install the price table used by
    /// [`Self::estimate_optional_cost_usd`]. Idempotent.
    pub fn install_price_table(&self, table: Arc<PriceTable>) {
        let _ = self.price_table.set(table);
    }

    /// Record one SC outcome — the AI handler calls this every
    /// time it fires sampling, regardless of whether the score
    /// crossed any threshold.
    pub fn record(&self, score: f32, sample_count: usize) {
        self.trigger_count.fetch_add(1, Ordering::Relaxed);
        self.total_samples
            .fetch_add(sample_count as u64, Ordering::Relaxed);
        // Rolling avg via a sum-of-bits encoding: we treat the
        // f32 as scaled-by-1e6 u64 and accumulate. Cheap, lossy
        // beyond ~1e6 calls, fine for an operator stat.
        let scaled = (score.clamp(0.0, 1.0) * 1_000_000.0) as u64;
        self.score_sum_bits.fetch_add(scaled, Ordering::Relaxed);
        self.score_count.fetch_add(1, Ordering::Relaxed);
        self.last_score_bits
            .store(score.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> SelfConsistencyStatsSnapshot {
        let count = self.score_count.load(Ordering::Relaxed);
        let avg = if count > 0 {
            let sum = self.score_sum_bits.load(Ordering::Relaxed);
            (sum as f64 / count as f64 / 1_000_000.0) as f32
        } else {
            0.0
        };
        SelfConsistencyStatsSnapshot {
            trigger_count: self.trigger_count.load(Ordering::Relaxed),
            total_samples: self.total_samples.load(Ordering::Relaxed),
            average_score: avg,
            last_score: f32::from_bits(self.last_score_bits.load(Ordering::Relaxed)),
        }
    }

    /// PART 3: `true` when the SC gate is currently disabled
    /// by a prior cost-guard trip.
    pub fn is_disabled(&self, now_unix_secs: i64) -> bool {
        let until = self.disabled_until_unix_secs.load(Ordering::Relaxed);
        until > now_unix_secs
    }

    /// PART 3: record one SC consideration outcome. The AI
    /// handler calls this every time it evaluates whether to
    /// fire SC — `triggered = true` when SC actually ran,
    /// `false` when SC was matched-but-skipped (baseline
    /// confidence above threshold, cost-budget exceeded, etc.).
    /// Rolls the ring forward; when the ring is full and the
    /// trigger rate exceeds `cfg.max_trigger_rate_pct`,
    /// disables SC for `cfg.disable_duration_secs` and emits
    /// a CostAlert.
    pub fn record_decision(
        &self,
        triggered: bool,
        now_unix_secs: i64,
        cfg: &SelfConsistencyConfig,
    ) {
        let trip = {
            let mut g = match self.guard.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if g.trigger_ring.len() >= TRIGGER_RING_CAPACITY {
                g.trigger_ring.pop_front();
            }
            g.trigger_ring.push_back(triggered);
            if g.trigger_ring.len() >= TRIGGER_RING_CAPACITY {
                let fired = g.trigger_ring.iter().filter(|b| **b).count();
                let rate_pct = (fired * 100) / g.trigger_ring.len();
                rate_pct > cfg.max_trigger_rate_pct as usize
            } else {
                false
            }
        };
        if trip {
            self.trip_disable(
                now_unix_secs,
                cfg.disable_duration_secs,
                "self_consistency_trigger_rate_exceeded",
            );
        }
    }

    /// PART 3: record the USD cost of one SC fan-out. Appends
    /// to the rolling hourly window and trips the hourly-budget
    /// guard when the window sum exceeds
    /// `cfg.sc_hourly_budget_usd`.
    pub fn record_sc_cost_usd(
        &self,
        cost_usd: f64,
        now_unix_secs: i64,
        cfg: &SelfConsistencyConfig,
    ) {
        let trip = {
            let mut g = match self.guard.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            g.hourly_spend_events.push_back((now_unix_secs, cost_usd));
            let cutoff = now_unix_secs - 3_600;
            while let Some(&(t, _)) = g.hourly_spend_events.front() {
                if t < cutoff {
                    g.hourly_spend_events.pop_front();
                } else {
                    break;
                }
            }
            let sum: f64 = g.hourly_spend_events.iter().map(|(_, c)| *c).sum();
            sum > cfg.sc_hourly_budget_usd
        };
        if trip {
            self.trip_disable(
                now_unix_secs,
                cfg.disable_duration_secs,
                "self_consistency_hourly_budget_exceeded",
            );
        }
    }

    /// PART 3: estimate the total optional-stage cost (SC
    /// samples + judge call + belief call) for one request,
    /// based on the baseline call's token usage. Returns
    /// `None` when no price table is installed or the model
    /// has no entry. Each optional stage is assumed to consume
    /// the same prompt + completion budget as the baseline
    /// call — a deliberate over-estimate so the budget gate
    /// fires before the actual spend lands.
    pub fn estimate_optional_cost_usd(
        &self,
        baseline_prompt_tokens: u32,
        baseline_completion_tokens: u32,
        model: &str,
        sample_count: usize,
    ) -> Option<f64> {
        let table = self.price_table.get()?;
        let base = table.estimate_cost_micros(
            model,
            baseline_prompt_tokens as u64,
            baseline_completion_tokens as u64,
        )?;
        // SC fan-out cost: `sample_count - 1` additional calls
        // (the baseline already happened and is NOT counted in
        // the "optional" bucket). Judge + belief add one each.
        let extra_sc_calls = sample_count.saturating_sub(1) as u64;
        let total_micros = base
            .saturating_mul(extra_sc_calls)
            .saturating_add(base.saturating_mul(2));
        Some(total_micros as f64 / 1_000_000.0)
    }

    fn trip_disable(&self, now_unix_secs: i64, duration_secs: u64, cause: &'static str) {
        let until = now_unix_secs.saturating_add(duration_secs as i64);
        // CAS so concurrent trips converge on the latest cutoff.
        let _ = self
            .disabled_until_unix_secs
            .fetch_max(until, Ordering::Relaxed);
        if let Some(sink) = self.alert_sink.get() {
            let event = AlertEvent::Fired(ActiveAlert {
                agent: "self_consistency".to_string(),
                kind: AlertKind::CostAlert,
                severity: AlertSeverity::Critical,
                triggered_at_ms: now_unix_secs.saturating_mul(1_000),
                threshold: 0.0,
                actual: 0.0,
                message: cause.to_string(),
                method: None,
            });
            sink.deliver(&event);
        }
        tracing::warn!(
            cause,
            disabled_until_unix_secs = until,
            "self_consistency: cost guard tripped — SC disabled"
        );
    }
}

/// JSON-serialisable rolling stats view returned by
/// `confidence.self_consistency_stats`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SelfConsistencyStatsSnapshot {
    /// How many times SC has triggered process-wide.
    pub trigger_count: u64,
    /// Total samples dispatched (sum of every outcome's
    /// `sample_count`).
    pub total_samples: u64,
    /// Running mean of every SC score seen.
    pub average_score: f32,
    /// Last SC score observed (NaN if no SC has run yet).
    pub last_score: f32,
}

/// Strip preamble openers and trim to the first `max_words`
/// whitespace-separated tokens. Mirrors the §7.29 spec's
/// "core answer" definition: most LLM answers contain a fluff
/// preamble ("Sure! Here's what I think...") before the actual
/// substantive content; SC measures whether the substantive
/// content *agrees* between samples, not whether the preambles
/// are similar.
pub fn extract_core_answer(text: &str, max_words: usize) -> String {
    let trimmed = strip_preamble(text);
    let mut iter = trimmed.split_whitespace();
    let mut out = String::with_capacity(trimmed.len().min(max_words * 8));
    for (i, w) in (&mut iter).enumerate() {
        if i >= max_words {
            break;
        }
        if i > 0 {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// Strip leading preamble lines that don't carry substantive
/// content. The heuristic: drop the first non-empty line if it
/// matches a known preamble opener (case-insensitive), then
/// trim leading whitespace.
fn strip_preamble(text: &str) -> &str {
    let trimmed = text.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    const OPENERS: &[&str] = &[
        "sure!",
        "sure,",
        "sure ",
        "of course!",
        "of course,",
        "of course ",
        "certainly!",
        "certainly,",
        "certainly ",
        "absolutely!",
        "absolutely,",
        "great question!",
        "great question.",
        "happy to help!",
        "here's",
        "here is",
        "let me",
        "i'll ",
        "i will ",
        "okay,",
        "ok,",
        "well,",
    ];
    for opener in OPENERS {
        if lower.starts_with(opener) {
            // Drop the rest of the opening sentence — find the
            // first `.`, `!`, `?`, or newline.
            if let Some(stop) = trimmed.find(['.', '!', '?', '\n']) {
                let after = &trimmed[stop + 1..];
                return after.trim_start();
            }
        }
    }
    trimmed
}

/// Cosine similarity in `[-1, 1]`. Returns `0.0` when either
/// vector is empty OR has zero magnitude (silently treats
/// degenerate input as orthogonal).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut mag_a = 0.0_f32;
    let mut mag_b = 0.0_f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        mag_a += a[i] * a[i];
        mag_b += b[i] * b[i];
    }
    if mag_a <= f32::EPSILON || mag_b <= f32::EPSILON {
        return 0.0;
    }
    (dot / (mag_a.sqrt() * mag_b.sqrt())).clamp(-1.0, 1.0)
}

/// Compute the per-sample coherence (avg cosine to every other
/// sample) AND the overall pairwise mean cosine.
///
/// `core_answers.len()` must equal `embeddings.len()`. With
/// fewer than two samples the function returns a default
/// `SelfConsistencyOutcome` with score = 0.0 — the AI handler
/// short-circuits earlier so this is only reachable as a
/// safety net.
pub fn evaluate_samples(
    core_answers: &[String],
    embeddings: &[Vec<f32>],
) -> SelfConsistencyOutcome {
    let n = embeddings.len();
    debug_assert_eq!(
        core_answers.len(),
        n,
        "core_answers and embeddings must align"
    );
    if n < 2 {
        return SelfConsistencyOutcome::default();
    }

    // Pre-compute the full cosine matrix once.
    let mut matrix = vec![vec![0.0_f32; n]; n];
    for i in 0..n {
        for j in (i + 1)..n {
            let sim = cosine_similarity(&embeddings[i], &embeddings[j]);
            matrix[i][j] = sim;
            matrix[j][i] = sim;
        }
    }

    let mut sum_total = 0.0_f32;
    let mut pair_count = 0_u32;
    let mut samples: Vec<SampleEvaluation> = Vec::with_capacity(n);
    let mut best_index = 0;
    let mut best_coherence = f32::MIN;
    for (i, row) in matrix.iter().enumerate() {
        let mut sum_i = 0.0_f32;
        for (j, sim) in row.iter().enumerate() {
            if i != j {
                sum_i += sim;
                if j > i {
                    sum_total += sim;
                    pair_count += 1;
                }
            }
        }
        let coh = if n > 1 { sum_i / (n - 1) as f32 } else { 0.0 };
        if coh > best_coherence {
            best_coherence = coh;
            best_index = i;
        }
        samples.push(SampleEvaluation {
            index: i,
            core_answer: core_answers[i].clone(),
            coherence: coh,
        });
    }
    let score = if pair_count > 0 {
        sum_total / pair_count as f32
    } else {
        0.0
    };
    SelfConsistencyOutcome {
        score: score.clamp(0.0, 1.0),
        best_index,
        samples,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_disabled() {
        let cfg = SelfConsistencyConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.sample_count, 3);
        assert!((cfg.min_score_to_enable - 0.70).abs() < f32::EPSILON);
    }

    #[test]
    fn matches_capability_handles_empty_pattern_list_as_everything() {
        let cfg = SelfConsistencyConfig {
            enabled: true,
            sample_count: 3,
            min_score_to_enable: 0.7,
            capability_patterns: Vec::new(),
            ..Default::default()
        };
        assert!(cfg.matches_capability("ai.chat"));
        assert!(cfg.matches_capability("anything.else"));
    }

    #[test]
    fn matches_capability_respects_globs() {
        let cfg = SelfConsistencyConfig {
            enabled: true,
            sample_count: 3,
            min_score_to_enable: 0.7,
            capability_patterns: vec!["ai.chat*".into(), "tool.search".into()],
            ..Default::default()
        };
        assert!(cfg.matches_capability("ai.chat"));
        assert!(cfg.matches_capability("ai.chat.stream"));
        assert!(cfg.matches_capability("tool.search"));
        assert!(!cfg.matches_capability("memory.read"));
    }

    #[test]
    fn matches_capability_rejects_when_disabled_or_sample_count_too_low() {
        let cfg = SelfConsistencyConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(!cfg.matches_capability("ai.chat"));
        let cfg = SelfConsistencyConfig {
            enabled: true,
            sample_count: 1,
            ..Default::default()
        };
        assert!(!cfg.matches_capability("ai.chat"));
    }

    #[test]
    fn should_trigger_only_below_threshold() {
        let cfg = SelfConsistencyConfig {
            enabled: true,
            sample_count: 3,
            min_score_to_enable: 0.7,
            capability_patterns: Vec::new(),
            ..Default::default()
        };
        assert!(cfg.should_trigger(0.5));
        assert!(!cfg.should_trigger(0.7));
        assert!(!cfg.should_trigger(0.9));
    }

    #[test]
    fn extract_core_answer_strips_preamble_and_truncates_to_word_limit() {
        let text = "Sure! Here's what I think about the topic. The capital of France is Paris and it has a population of about 2 million.";
        let core = extract_core_answer(text, 10);
        assert!(!core.is_empty(), "got empty: {core:?}");
        assert!(
            !core.to_ascii_lowercase().starts_with("sure"),
            "preamble leaked: {core}"
        );
        assert!(core.split_whitespace().count() <= 10);
    }

    #[test]
    fn extract_core_answer_keeps_text_when_no_preamble_present() {
        let text = "The capital of France is Paris.";
        let core = extract_core_answer(text, 100);
        assert_eq!(core, "The capital of France is Paris.");
    }

    #[test]
    fn cosine_similarity_handles_identical_vectors() {
        let a = vec![1.0_f32, 2.0, 3.0];
        assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_similarity_handles_orthogonal_vectors() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-5);
    }

    #[test]
    fn cosine_similarity_returns_zero_for_zero_magnitude() {
        let a = vec![0.0_f32, 0.0];
        let b = vec![1.0_f32, 1.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_similarity_returns_zero_for_mismatched_lengths() {
        let a = vec![1.0_f32];
        let b = vec![1.0_f32, 1.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn evaluate_samples_with_identical_embeddings_yields_high_score() {
        let answers = vec!["alpha".into(), "alpha".into(), "alpha".into()];
        let embeds = vec![vec![1.0, 2.0, 3.0]; 3];
        let out = evaluate_samples(&answers, &embeds);
        assert!(
            (out.score - 1.0).abs() < 1e-5,
            "expected ~1.0 got {}",
            out.score
        );
        assert_eq!(out.samples.len(), 3);
    }

    #[test]
    fn evaluate_samples_with_orthogonal_embeddings_yields_low_score() {
        let answers = vec!["a".into(), "b".into(), "c".into()];
        let embeds = vec![
            vec![1.0_f32, 0.0, 0.0],
            vec![0.0_f32, 1.0, 0.0],
            vec![0.0_f32, 0.0, 1.0],
        ];
        let out = evaluate_samples(&answers, &embeds);
        assert!(out.score.abs() < 1e-5, "expected ~0.0 got {}", out.score);
    }

    #[test]
    fn evaluate_samples_picks_the_most_coherent_index() {
        // Three samples; two are aligned, one is the odd one
        // out. Expect best_index ∈ {0, 1} (either of the
        // aligned pair).
        let answers = vec!["a".into(), "b".into(), "c".into()];
        let embeds = vec![vec![1.0_f32, 0.0], vec![1.0_f32, 0.05], vec![0.0_f32, 1.0]];
        let out = evaluate_samples(&answers, &embeds);
        assert!(
            out.best_index == 0 || out.best_index == 1,
            "best should be one of the aligned pair, got {}",
            out.best_index
        );
    }

    #[test]
    fn evaluate_samples_with_fewer_than_two_returns_default() {
        let out = evaluate_samples(&[String::from("x")], &[vec![1.0_f32, 0.0]]);
        assert_eq!(out.score, 0.0);
        assert_eq!(out.best_index, 0);
        assert!(out.samples.is_empty());
    }
}
