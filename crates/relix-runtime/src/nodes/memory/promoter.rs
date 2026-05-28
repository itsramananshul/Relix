//! Layer promotion loop — moves memory records up the
//! four-layer hierarchy (Raw → Semantic → Observation → Model).
//!
//! Distinct from the existing
//! [`crate::nodes::memory::curator`] subsystem, which
//! consolidates per-subject `agent` + `user` memory blobs. The
//! promoter operates on the layered `memory_records` table
//! introduced in W2-MEMORY-LAYERED; the two systems share a
//! `[memory.curator]` config section but run independently.
//!
//! ## Stages
//!
//! 1. **Raw → Semantic** — fetch Raw records that have an
//!    embedding, group them by `source` (session id), drop
//!    near-duplicates by cosine similarity, and write the
//!    survivors as Semantic records. Source Raw records get a
//!    `promoted:semantic` tag so the same record is not
//!    re-promoted on the next tick.
//! 2. **Semantic → Observation** — fetch un-promoted Semantic
//!    records, hand them to the LLM with an "extract
//!    observations" prompt, parse the dash-prefixed reply into
//!    individual Observation records.
//! 3. **Observation → Model** — for each source with active
//!    Observation records, hand them to the LLM with a
//!    "synthesize a living model" prompt. Invalidate the
//!    previous Model record (`valid_to = now`) before writing
//!    the replacement, so the bi-temporal history is preserved.
//!    Rate-limited to at most one Model regeneration per source
//!    per hour.
//!
//! ## Failure posture
//!
//! Every stage logs warns and continues — a stuck record or a
//! transient AI failure must never wedge the queue. The
//! `run_once` test surface returns the per-stage counts so
//! tests can assert behaviour deterministically.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;

use super::anomaly::{AnomalyAction, score_observation};
use super::guard::MemoryGuard;
use super::schema::{LayeredMemoryStore, MemoryLayer, MemoryRecord, SourceTrust};

/// `Fn(prompt) -> reply` shaped function the promoter uses to
/// reach the LLM. Production wires this to the same `ai.chat`
/// dispatcher the curator uses; tests inject a stub.
pub type PromoterAiFn =
    Arc<dyn Fn(String) -> BoxFuture<'static, Result<String, String>> + Send + Sync>;

/// Cosine-similarity threshold above which two Semantic
/// records are considered duplicates. 0.95 is conservative —
/// "essentially the same sentence" with room for paraphrase.
pub const DEDUP_COSINE_THRESHOLD: f32 = 0.95;

/// Maximum age of a Model record before regeneration is
/// allowed for the same source. Mirrors the spec's "at most
/// once per hour per source" rule.
pub const MODEL_THROTTLE_SECS: i64 = 3600;

/// Tag stamped on a Raw record once it has been promoted to
/// Semantic. Public so the bridge / inspector can surface it.
pub const PROMOTED_SEMANTIC_TAG: &str = "promoted:semantic";

/// Tag stamped on a Semantic record once observations have
/// been extracted from it.
pub const PROMOTED_OBSERVATION_TAG: &str = "promoted:observation";

/// Marker tag set on every record the promoter creates so the
/// inspector can tell "this came from the promotion loop" from
/// "the operator wrote this".
pub const AUTO_GENERATED_TAG: &str = "auto:promoter";

/// Per-tick counts returned by [`LayerPromoter::run_once`] —
/// makes assertions cheap in tests and gives the operator a
/// real progress signal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromotionStats {
    pub raw_to_semantic: usize,
    pub semantic_to_observation: usize,
    pub observations_to_model: usize,
    pub poisoned_skipped: usize,
}

/// The promotion loop. Cheap to construct; `spawn()` actually
/// starts the background task.
pub struct LayerPromoter {
    store: Arc<LayeredMemoryStore>,
    ai_fn: PromoterAiFn,
    batch_size: usize,
    /// RELIX-7.15 PII defense-in-depth: every promoted-record
    /// text is anonymized BEFORE insert. The Layer 1 source
    /// records are already anonymized by the recorder, but
    /// the LLM-driven `Semantic → Observation` and
    /// `Observation → Model` stages might hallucinate a value
    /// the source never carried; this pass catches that.
    anonymizer: Arc<crate::training::PiiAnonymizer>,
}

impl LayerPromoter {
    /// Construct a promoter with anonymization disabled.
    /// Existing callers + tests keep this shape.
    pub fn new(store: Arc<LayeredMemoryStore>, ai_fn: PromoterAiFn, batch_size: usize) -> Self {
        Self::new_with_anonymizer(
            store,
            ai_fn,
            batch_size,
            Arc::new(crate::training::PiiAnonymizer::disabled()),
        )
    }

    /// Construct a promoter with an explicit anonymizer.
    pub fn new_with_anonymizer(
        store: Arc<LayeredMemoryStore>,
        ai_fn: PromoterAiFn,
        batch_size: usize,
        anonymizer: Arc<crate::training::PiiAnonymizer>,
    ) -> Self {
        Self {
            store,
            ai_fn,
            batch_size: batch_size.max(1),
            anonymizer,
        }
    }

    /// Anonymize `text` iff the per-promoter anonymizer is
    /// enabled. Returns a copy in either case — the caller
    /// always owns the resulting string.
    fn anon(&self, text: &str) -> String {
        if self.anonymizer.enabled() {
            self.anonymizer.anonymize(text)
        } else {
            text.to_string()
        }
    }

    /// Run one promotion tick. Stages execute in sequence so
    /// records that move Raw → Semantic in this tick are also
    /// eligible for Semantic → Observation on the same tick.
    pub async fn run_once(&self) -> PromotionStats {
        let mut stats = PromotionStats::default();
        match self.promote_raw_to_semantic().await {
            Ok(s) => {
                stats.raw_to_semantic = s.promoted;
                stats.poisoned_skipped += s.poisoned;
            }
            Err(e) => tracing::warn!(error = %e, "promoter: Raw → Semantic failed"),
        }
        match self.promote_semantic_to_observation().await {
            Ok(s) => {
                stats.semantic_to_observation = s.promoted;
                stats.poisoned_skipped += s.poisoned;
            }
            Err(e) => tracing::warn!(error = %e, "promoter: Semantic → Observation failed"),
        }
        match self.promote_observations_to_model().await {
            Ok(n) => stats.observations_to_model = n,
            Err(e) => tracing::warn!(error = %e, "promoter: Observation → Model failed"),
        }
        stats
    }

    /// Raw → Semantic. Pure-Rust dedupe pass — no LLM call.
    pub async fn promote_raw_to_semantic(&self) -> Result<StageOutcome, String> {
        let mut outcome = StageOutcome::default();
        let raws = self.fetch_unpromoted(MemoryLayer::Raw, PROMOTED_SEMANTIC_TAG)?;
        if raws.is_empty() {
            return Ok(outcome);
        }
        // Group by source so dedup happens within a session
        // rather than across (a user saying "hello" in two
        // different sessions is two facts, not one).
        let mut by_source: BTreeMap<String, Vec<MemoryRecord>> = BTreeMap::new();
        for r in raws {
            by_source.entry(r.source.clone()).or_default().push(r);
        }
        let now = unix_secs();
        for (source, group) in by_source {
            let mut kept: Vec<MemoryRecord> = Vec::new();
            for r in group {
                if MemoryGuard::is_poisoned(&r.text) {
                    outcome.poisoned += 1;
                    let _ = self.store.invalidate(&r.id, now);
                    tracing::warn!(
                        record_id = %r.id,
                        "promoter: Raw record marked poisoned; invalidated instead of promoted"
                    );
                    continue;
                }
                let r_embed = match &r.embedding {
                    Some(v) => v.clone(),
                    None => continue,
                };
                if kept.iter().any(|prev| match &prev.embedding {
                    Some(pv) => cosine(&r_embed, pv) >= DEDUP_COSINE_THRESHOLD,
                    None => false,
                }) {
                    // Near-duplicate of an already-kept record;
                    // still stamp the source as promoted so we
                    // don't reconsider it next tick.
                    let _ = self.store.add_tag(&r.id, PROMOTED_SEMANTIC_TAG);
                    continue;
                }
                kept.push(r);
            }
            for survivor in kept {
                let sem = build_promoted_record(
                    &survivor,
                    MemoryLayer::Semantic,
                    self.anon(&survivor.text),
                    &source,
                );
                if let Err(e) = self.store.insert(&sem) {
                    tracing::warn!(error = %e, "promoter: insert Semantic failed");
                    continue;
                }
                outcome.promoted += 1;
                let _ = self.store.add_tag(&survivor.id, PROMOTED_SEMANTIC_TAG);
            }
        }
        Ok(outcome)
    }

    /// Semantic → Observation. Calls the LLM once per batch
    /// and parses dash-prefixed lines as observations.
    pub async fn promote_semantic_to_observation(&self) -> Result<StageOutcome, String> {
        let mut outcome = StageOutcome::default();
        let semantics = self.fetch_unpromoted(MemoryLayer::Semantic, PROMOTED_OBSERVATION_TAG)?;
        if semantics.is_empty() {
            return Ok(outcome);
        }
        // Group by source so the LLM sees one source at a time
        // — observations are per-subject, mixing sessions
        // dilutes the signal.
        let mut by_source: BTreeMap<String, Vec<MemoryRecord>> = BTreeMap::new();
        for r in semantics {
            by_source.entry(r.source.clone()).or_default().push(r);
        }
        let now = unix_secs();
        for (source, group) in by_source {
            let bulk_text: String = group
                .iter()
                .enumerate()
                .map(|(i, r)| format!("{}. {}", i + 1, r.text.trim()))
                .collect::<Vec<_>>()
                .join("\n");
            let prompt = format!(
                "Extract factual observations from these memory chunks. \
                 Return one observation per line, starting with '-'. Be concise.\n\
                 Chunks:\n{bulk_text}"
            );
            let reply = match (self.ai_fn)(prompt).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        source = %source,
                        error = %e,
                        "promoter: AI observation extraction failed (silent skip)"
                    );
                    continue;
                }
            };
            let observations: Vec<String> = reply
                .lines()
                .map(|l| l.trim())
                .filter_map(|l| l.strip_prefix('-').map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect();
            // GAP 6: snapshot existing observations for this
            // source so the anomaly scorer can detect
            // contradictions against prior writes.
            let existing_observations = self
                .store
                .list(
                    Some(MemoryLayer::Observation),
                    Some(source.as_str()),
                    1000,
                    0,
                )
                .unwrap_or_default()
                .into_iter()
                .filter(|r| r.valid_to.is_none())
                .collect::<Vec<_>>();
            // Source trust inherits from the parent semantic
            // records — if any parent was external, the
            // observations they yielded are external too.
            let inherited_trust = group
                .iter()
                .map(|r| r.source_trust)
                .max_by_key(|t| match t {
                    SourceTrust::External => 2,
                    SourceTrust::Unknown => 1,
                    SourceTrust::Internal => 0,
                })
                .unwrap_or(SourceTrust::Internal);
            for (i, obs_text) in observations.iter().enumerate() {
                if MemoryGuard::is_poisoned(obs_text) {
                    outcome.poisoned += 1;
                    tracing::warn!(
                        source = %source,
                        index = i,
                        "promoter: extracted observation marked poisoned; skipped"
                    );
                    continue;
                }
                let parent_id = group.first().map(|r| r.id.as_str()).unwrap_or("");
                let id = mint_promoted_id(parent_id, MemoryLayer::Observation, i);
                let scrubbed = self.anon(obs_text);
                let mut record = MemoryRecord::new_raw(id, scrubbed, source.clone());
                record.layer = MemoryLayer::Observation;
                record.created_at = now;
                record.observed_at = now;
                record.valid_from = now;
                record.tags = vec![AUTO_GENERATED_TAG.to_string()];
                record.source_trust = inherited_trust;

                // GAP 6: write-time anomaly scoring.
                let anomaly = score_observation(&record.text, &existing_observations);
                match anomaly.action() {
                    AnomalyAction::Reject => {
                        outcome.anomaly_rejected += 1;
                        tracing::warn!(
                            source = %source,
                            index = i,
                            reason = %anomaly.reason_line(),
                            score = anomaly.score,
                            "promoter: extracted observation rejected by anomaly scorer"
                        );
                        continue;
                    }
                    AnomalyAction::Quarantine => {
                        let id = quarantine_id_for(&record, anomaly.score);
                        let json = match serde_json::to_string(&record) {
                            Ok(j) => j,
                            Err(e) => {
                                tracing::warn!(error = %e, "promoter: quarantine encode failed");
                                continue;
                            }
                        };
                        if let Err(e) = self.store.quarantine_insert(
                            &id,
                            &json,
                            &anomaly.reason_line(),
                            now * 1000,
                            inherited_trust,
                        ) {
                            tracing::warn!(error = %e, "promoter: quarantine insert failed");
                            continue;
                        }
                        outcome.quarantined += 1;
                        tracing::warn!(
                            source = %source,
                            index = i,
                            reason = %anomaly.reason_line(),
                            score = anomaly.score,
                            quarantine_id = %id,
                            "promoter: extracted observation routed to quarantine"
                        );
                        continue;
                    }
                    AnomalyAction::Accept => {}
                }

                if let Err(e) = self.store.insert(&record) {
                    tracing::warn!(error = %e, "promoter: insert Observation failed");
                    continue;
                }
                outcome.promoted += 1;
            }
            for sem in &group {
                let _ = self.store.add_tag(&sem.id, PROMOTED_OBSERVATION_TAG);
            }
        }
        Ok(outcome)
    }

    /// Observation → Model. Rate-limited per source.
    pub async fn promote_observations_to_model(&self) -> Result<usize, String> {
        // Enumerate sources that have valid Observation records.
        let observations = self.store_list_valid(MemoryLayer::Observation)?;
        let mut by_source: BTreeMap<String, Vec<MemoryRecord>> = BTreeMap::new();
        for r in observations {
            by_source.entry(r.source.clone()).or_default().push(r);
        }
        if by_source.is_empty() {
            return Ok(0);
        }
        let now = unix_secs();
        let mut written = 0usize;
        for (source, observations) in by_source {
            // Throttle: skip if the current Model for this
            // source is younger than MODEL_THROTTLE_SECS.
            let prev = self
                .store
                .latest_by_layer_and_source(MemoryLayer::Model, &source)
                .map_err(|e| e.to_string())?;
            if let Some(prev) = &prev
                && now - prev.observed_at < MODEL_THROTTLE_SECS
                && prev.valid_to.is_none()
            {
                continue;
            }
            let bulk_text = observations
                .iter()
                .enumerate()
                .map(|(i, r)| format!("{}. {}", i + 1, r.text.trim()))
                .collect::<Vec<_>>()
                .join("\n");
            let prompt = format!(
                "Synthesize these observations into a living model of this agent/user. \
                 Return a structured summary with sections: Goals, Preferences, \
                 Constraints, Knowledge. Be concise.\n\
                 Observations:\n{bulk_text}"
            );
            let reply = match (self.ai_fn)(prompt).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        source = %source,
                        error = %e,
                        "promoter: AI model synthesis failed (silent skip)"
                    );
                    continue;
                }
            };
            // Mint a new id derived from source + observed_at
            // so retries with the same observed_at second don't
            // create churn under the same source.
            let id = mint_promoted_id(&source, MemoryLayer::Model, now as usize);
            // Invalidate the previous Model before inserting
            // the new one so bi-temporal history is preserved.
            if let Some(prev) = prev
                && prev.valid_to.is_none()
                && let Err(e) = self.store.invalidate(&prev.id, now)
            {
                tracing::warn!(error = %e, source = %source, "promoter: invalidate prior Model failed");
            }
            let scrubbed_reply = self.anon(&reply);
            let mut record = MemoryRecord::new_raw(id, scrubbed_reply, source.clone());
            record.layer = MemoryLayer::Model;
            record.created_at = now;
            record.observed_at = now;
            record.valid_from = now;
            record.tags = vec![AUTO_GENERATED_TAG.to_string()];
            if let Err(e) = self.store.insert(&record) {
                tracing::warn!(error = %e, "promoter: insert Model failed");
                continue;
            }
            written += 1;
        }
        Ok(written)
    }

    fn fetch_unpromoted(
        &self,
        layer: MemoryLayer,
        marker_tag: &str,
    ) -> Result<Vec<MemoryRecord>, String> {
        // Generous limit on the SQL list so we can apply the
        // promoter's batch_size after filtering in Rust.
        let raw = self
            .store
            .list(Some(layer), None, self.batch_size.saturating_mul(4), 0)
            .map_err(|e| e.to_string())?;
        let mut out: Vec<MemoryRecord> = Vec::with_capacity(self.batch_size);
        for r in raw {
            if r.embedding.is_none() && layer == MemoryLayer::Raw {
                continue;
            }
            if r.tags.iter().any(|t| t == marker_tag) {
                continue;
            }
            if r.valid_to.is_some() {
                continue;
            }
            out.push(r);
            if out.len() >= self.batch_size {
                break;
            }
        }
        Ok(out)
    }

    fn store_list_valid(&self, layer: MemoryLayer) -> Result<Vec<MemoryRecord>, String> {
        let raw = self
            .store
            .list(Some(layer), None, 1_000, 0)
            .map_err(|e| e.to_string())?;
        Ok(raw.into_iter().filter(|r| r.valid_to.is_none()).collect())
    }
}

/// Per-stage outcome — separates "successfully promoted" from
/// "rejected by the memory guard."
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StageOutcome {
    pub promoted: usize,
    pub poisoned: usize,
    /// GAP 6: count of candidates routed to the
    /// `memory_quarantine` table by the [`super::anomaly`]
    /// scorer (write-time, not detected as a poisoning pattern,
    /// but anomalous enough to require operator review).
    pub quarantined: usize,
    /// GAP 6: count of candidates hard-rejected by the anomaly
    /// scorer.
    pub anomaly_rejected: usize,
}

/// Build a child record carrying the parent's metadata + the
/// `auto:promoter` tag so the inspector can identify it.
fn build_promoted_record(
    parent: &MemoryRecord,
    new_layer: MemoryLayer,
    text: String,
    source: &str,
) -> MemoryRecord {
    let now = unix_secs();
    let id = mint_promoted_id(&parent.id, new_layer, 0);
    let mut tags = vec![AUTO_GENERATED_TAG.to_string()];
    // Carry forward parent tags that aren't markers so
    // downstream filters see them.
    for t in &parent.tags {
        if t == PROMOTED_SEMANTIC_TAG || t == PROMOTED_OBSERVATION_TAG || t == AUTO_GENERATED_TAG {
            continue;
        }
        tags.push(t.clone());
    }
    MemoryRecord {
        id,
        layer: new_layer,
        text,
        source: source.to_string(),
        tags,
        created_at: now,
        valid_from: now,
        valid_to: None,
        observed_at: now,
        embedding: None,
        // RELIX-7.16: promoted records inherit the parent's
        // shareable flag + share_policy so an operator who
        // marks a Raw record `share_policy = "auto"` gets that
        // posture carried up the Semantic / Observation /
        // Model chain. `shared_with` + `shared_by` reset on
        // promotion — those are per-share-event metadata, not
        // structural lineage.
        shareable: parent.shareable,
        shared_with: Vec::new(),
        shared_by: None,
        share_policy: parent.share_policy,
        // RELIX-MEM: derived records inherit the parent's
        // source-trust and freeze posture; consolidation +
        // edit timestamps start fresh on every promotion.
        source_trust: parent.source_trust,
        frozen: parent.frozen,
        last_edited_ms: None,
        consolidated: false,
        // GAP 23: promoted records inherit the parent's
        // tenant so the per-tenant Qdrant collection stays
        // consistent across the Raw → Semantic → Observation
        // → Model promotion chain.
        tenant_id: parent.tenant_id.clone(),
        // GAP 18: a freshly-promoted record starts its own
        // bi-temporal chain. Supersedes is a per-fact
        // pointer, not a layer-promotion concept.
        superseded_by: None,
    }
}

fn mint_promoted_id(parent_id: &str, layer: MemoryLayer, suffix: usize) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(parent_id.as_bytes());
    hasher.update(b"|");
    hasher.update(layer.as_str().as_bytes());
    hasher.update(b"|");
    hasher.update(suffix.to_le_bytes().as_ref());
    hasher.finalize().to_hex().as_str()[..16].to_string()
}

/// GAP 6: derive a quarantine row id from the candidate record
/// plus the anomaly score. Stable across retries (so a duplicate
/// scoring call upserts instead of inflating the row count).
fn quarantine_id_for(record: &MemoryRecord, score: f32) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(record.id.as_bytes());
    hasher.update(b"|");
    hasher.update(record.text.as_bytes());
    hasher.update(b"|");
    hasher.update(record.source.as_bytes());
    hasher.update(b"|");
    hasher.update(score.to_le_bytes().as_ref());
    format!("q.{}", hex::encode(&hasher.finalize().as_bytes()[..8]))
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Spawn the periodic promotion loop. Returns the
/// [`tokio::task::JoinHandle`] so the controller can keep it
/// alive (or drop to detach).
pub fn spawn_promotion_loop(
    promoter: LayerPromoter,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tracing::info!(
            interval_secs = interval.as_secs(),
            "memory promoter: layer-promotion loop started"
        );
        loop {
            let _ = promoter.run_once().await;
            tokio::time::sleep(interval).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::memory::schema::{LayeredMemoryStore, MemoryLayer, MemoryRecord};

    fn store_with_raws(rows: &[(&str, &str, Vec<f32>)]) -> Arc<LayeredMemoryStore> {
        let store = LayeredMemoryStore::in_memory().unwrap();
        for (id, text, embed) in rows {
            let mut r = MemoryRecord::new_raw(*id, *text, "sess-1");
            r.embedding = Some(embed.clone());
            store.insert(&r).unwrap();
        }
        Arc::new(store)
    }

    fn ai_returning(reply: &str) -> PromoterAiFn {
        let reply = reply.to_string();
        Arc::new(move |_prompt: String| {
            let reply = reply.clone();
            Box::pin(async move { Ok(reply) }) as BoxFuture<'static, Result<String, String>>
        })
    }

    fn ai_failing() -> PromoterAiFn {
        Arc::new(|_prompt: String| {
            Box::pin(async move { Err("ai.chat unreachable".to_string()) })
                as BoxFuture<'static, Result<String, String>>
        })
    }

    #[tokio::test]
    async fn raw_to_semantic_dedups_near_identical_vectors() {
        // Two near-identical vectors (cosine ≈ 1.0) and one
        // distinct vector. Dedup should keep one + one.
        let store = store_with_raws(&[
            ("a", "deploy staging", vec![1.0, 0.0, 0.0, 0.0]),
            ("b", "deploy staging please", vec![0.999, 0.001, 0.0, 0.0]),
            ("c", "make coffee", vec![0.0, 0.0, 1.0, 0.0]),
        ]);
        let promoter = LayerPromoter::new(store.clone(), ai_failing(), 10);
        let s = promoter.promote_raw_to_semantic().await.expect("ok");
        assert_eq!(s.promoted, 2, "expected one survivor per cluster");
        assert_eq!(s.poisoned, 0);
        // Semantic records exist; raw records carry the
        // promoted marker (all three, including the deduped one).
        let semantics = store
            .list(Some(MemoryLayer::Semantic), Some("sess-1"), 10, 0)
            .unwrap();
        assert_eq!(semantics.len(), 2);
        for raw in store
            .list(Some(MemoryLayer::Raw), Some("sess-1"), 10, 0)
            .unwrap()
        {
            assert!(
                raw.tags.iter().any(|t| t == PROMOTED_SEMANTIC_TAG),
                "raw {} should be marked promoted",
                raw.id
            );
        }
    }

    #[tokio::test]
    async fn semantic_to_observation_parses_ai_reply_into_records() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        let mut r = MemoryRecord::new_raw("s1", "hello world", "session-x");
        r.layer = MemoryLayer::Semantic;
        r.embedding = Some(vec![1.0]);
        store.insert(&r).unwrap();
        // GAP 6: every dash-prefixed line carries at least one
        // proper-noun token (Postgres, EST, Rust) so the anomaly
        // scorer accepts all three and they all land as
        // observations.
        let ai_reply = "- The user prefers Postgres in production\n\
                        - The user works in EST\n\
                           - The user uses Rust";
        let promoter = LayerPromoter::new(Arc::new(store.clone()), ai_returning(ai_reply), 10);
        let s = promoter
            .promote_semantic_to_observation()
            .await
            .expect("ok");
        assert_eq!(s.promoted, 3, "three dash-prefixed lines");
        let observations = store
            .list(Some(MemoryLayer::Observation), Some("session-x"), 10, 0)
            .unwrap();
        assert_eq!(observations.len(), 3);
        let texts: Vec<&str> = observations.iter().map(|r| r.text.as_str()).collect();
        assert!(texts.iter().any(|t| t.contains("Postgres")));
        assert!(texts.iter().any(|t| t.contains("EST")));
        let semantics = store
            .list(Some(MemoryLayer::Semantic), Some("session-x"), 10, 0)
            .unwrap();
        assert!(
            semantics[0]
                .tags
                .iter()
                .any(|t| t == PROMOTED_OBSERVATION_TAG)
        );
    }

    #[tokio::test]
    async fn observation_to_model_invalidates_prior_and_writes_new() {
        let store_arc = Arc::new(LayeredMemoryStore::in_memory().unwrap());
        // Existing Model from "a while ago" — must be
        // invalidated AND replaced (its age is past the
        // throttle so the synthesis is allowed to run).
        let mut prior_model = MemoryRecord::new_raw("old-model", "OLD MODEL TEXT", "agent-x");
        prior_model.layer = MemoryLayer::Model;
        prior_model.observed_at -= MODEL_THROTTLE_SECS * 2;
        prior_model.created_at = prior_model.observed_at;
        prior_model.valid_from = prior_model.observed_at;
        store_arc.insert(&prior_model).unwrap();
        // One valid Observation.
        let mut obs = MemoryRecord::new_raw("obs-1", "user lives in Tokyo", "agent-x");
        obs.layer = MemoryLayer::Observation;
        store_arc.insert(&obs).unwrap();
        let promoter = LayerPromoter::new(
            store_arc.clone(),
            ai_returning("Goals: ship faster\nPreferences: terse"),
            10,
        );
        let n = promoter.promote_observations_to_model().await.expect("ok");
        assert_eq!(n, 1, "exactly one new Model written");
        let prior = store_arc.get("old-model").unwrap().unwrap();
        assert!(
            prior.valid_to.is_some(),
            "previous Model must be invalidated"
        );
        let current = store_arc
            .latest_by_layer_and_source(MemoryLayer::Model, "agent-x")
            .unwrap()
            .expect("new model present");
        assert!(current.valid_to.is_none(), "new Model must be valid");
        assert!(current.text.contains("Goals: ship faster"));
    }

    #[tokio::test]
    async fn observation_to_model_throttles_within_an_hour() {
        let store_arc = Arc::new(LayeredMemoryStore::in_memory().unwrap());
        // A "fresh" Model exists (created_at = now). One
        // valid Observation also exists. The synthesis should
        // be skipped because the throttle window has not
        // elapsed.
        let mut model = MemoryRecord::new_raw("m", "STATE", "agent-y");
        model.layer = MemoryLayer::Model;
        store_arc.insert(&model).unwrap();
        let mut obs = MemoryRecord::new_raw("o", "fact", "agent-y");
        obs.layer = MemoryLayer::Observation;
        store_arc.insert(&obs).unwrap();
        let calls = Arc::new(std::sync::Mutex::new(0u32));
        let calls_clone = calls.clone();
        let ai: PromoterAiFn = Arc::new(move |_p| {
            let calls = calls_clone.clone();
            Box::pin(async move {
                *calls.lock().unwrap() += 1;
                Ok("NEW".to_string())
            }) as BoxFuture<'static, Result<String, String>>
        });
        let promoter = LayerPromoter::new(store_arc.clone(), ai, 10);
        let n = promoter.promote_observations_to_model().await.expect("ok");
        assert_eq!(n, 0);
        assert_eq!(*calls.lock().unwrap(), 0, "AI must not be called");
    }

    #[tokio::test]
    async fn run_once_collects_per_stage_counts() {
        let store_arc = store_with_raws(&[
            ("a", "fact one", vec![1.0, 0.0]),
            ("b", "fact two", vec![0.0, 1.0]),
        ]);
        // GAP 6: each observation needs at least one specific
        // token so the anomaly scorer doesn't quarantine it.
        let promoter = LayerPromoter::new(
            store_arc.clone(),
            ai_returning("- The user uses Postgres for storage\n- The user works on Rust services"),
            10,
        );
        let stats = promoter.run_once().await;
        assert_eq!(stats.raw_to_semantic, 2);
        // The newly-minted Semantic records get processed in
        // the SAME tick because stages run in sequence.
        assert_eq!(stats.semantic_to_observation, 2);
        // No Model yet because no prior — first run is allowed.
        assert_eq!(stats.observations_to_model, 1);
    }

    #[tokio::test]
    async fn spawn_promotion_loop_returns_running_handle() {
        let store = Arc::new(LayeredMemoryStore::in_memory().unwrap());
        let promoter = LayerPromoter::new(store, ai_failing(), 10);
        let handle = spawn_promotion_loop(promoter, Duration::from_secs(60));
        // A freshly-spawned loop is still running — assert
        // the handle isn't aborted/finished immediately.
        assert!(!handle.is_finished());
        handle.abort();
    }

    #[test]
    fn cosine_matches_known_values() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        // Mismatched lengths return 0.
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0);
        // Zero vectors return 0 (no divide-by-zero).
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }
}
