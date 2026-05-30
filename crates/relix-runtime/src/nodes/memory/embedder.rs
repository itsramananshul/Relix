//! Background embedding pipeline for the four-layer memory
//! store.
//!
//! The pipeline runs as a tokio task and processes
//! [`MemoryRecord`]s whose `embedding` column is still `NULL`.
//! Per tick it batches up to `batch_size` rows, hands the
//! texts to the operator-supplied `ai_embed_fn` (which dials
//! the AI peer's `ai.embed` capability in production, or
//! returns canned vectors in tests), stamps the embedding on
//! the SQLite row, and — when a Qdrant client is wired —
//! upserts the vector + payload into the vector store.
//!
//! ## Failure posture
//!
//! Partial failures fold into a `tracing::warn!` and the
//! pipeline moves on to the next record. The whole point of
//! running this as a background loop is that "best effort,
//! reattempt next tick" is the right shape — we don't want
//! one stuck record to wedge the queue.
//!
//! ## Wire shape between pipeline and AI node
//!
//! `ai_embed_fn` is deliberately a `Fn(Vec<String>) ->
//! BoxFuture<...>` so tests can inject a stub without dragging
//! in libp2p. Production wiring in `controller_runtime` builds
//! the closure as a thin shim over the same
//! `EmbeddingDispatcher` the existing `memory.search` /
//! `memory.embed` handlers consume.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::Deserialize;

use super::qdrant::{QdrantClient, QdrantPoint};
use super::schema::{LayeredMemoryStore, MemoryRecord, qdrant_point_id_from_str};

/// `[memory.embedder]` config block. Absent or `enabled =
/// false` means the pipeline does not spawn.
#[derive(Clone, Debug, Deserialize)]
pub struct EmbedderConfig {
    /// Master switch.
    #[serde(default)]
    pub enabled: bool,
    /// Max rows handed to the AI embed call per tick. Default
    /// 32 matches the chunk size used by the existing
    /// `memory.embed_all` handler.
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    /// Seconds between ticks. Default 60.
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
    /// Cosine-similarity floor the memory.search handler will
    /// later apply against Qdrant results. Lives here so the
    /// operator can tune it in one place.
    #[serde(default = "default_score_threshold")]
    pub score_threshold: f32,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            batch_size: default_batch_size(),
            interval_secs: default_interval_secs(),
            score_threshold: default_score_threshold(),
        }
    }
}

fn default_batch_size() -> usize {
    32
}

fn default_interval_secs() -> u64 {
    60
}

fn default_score_threshold() -> f32 {
    0.75
}

/// Trait-object alias for the operator-injected embed
/// function. Takes a batch of texts; returns a batch of
/// vectors of the SAME length (one per text, in order).
pub type EmbedFn =
    Arc<dyn Fn(Vec<String>) -> BoxFuture<'static, Result<Vec<Vec<f32>>, String>> + Send + Sync>;

/// Background pipeline. Cheap to construct; `spawn()` is what
/// actually starts the loop.
pub struct EmbeddingPipeline {
    store: Arc<LayeredMemoryStore>,
    qdrant: Option<Arc<QdrantClient>>,
    ai_embed_fn: EmbedFn,
    batch_size: usize,
    interval: Duration,
    /// RELIX-7.15 PII anonymizer applied to every pending
    /// record's text BEFORE it's handed to the embed function
    /// AND before it lands on the Qdrant payload. The
    /// recorder already anonymizes at write_turn time so this
    /// is a defense-in-depth pass — any record that landed in
    /// the store via a code path that bypassed the recorder
    /// still gets scrubbed here, and the embed call NEVER
    /// sees a raw PII value when `[memory.pii]` is enabled.
    anonymizer: Arc<crate::training::PiiAnonymizer>,
}

impl EmbeddingPipeline {
    /// Construct a pipeline with PII anonymization disabled.
    /// Existing callers + tests keep this shape.
    pub fn new(
        store: Arc<LayeredMemoryStore>,
        qdrant: Option<Arc<QdrantClient>>,
        ai_embed_fn: EmbedFn,
        batch_size: usize,
        interval_secs: u64,
    ) -> Self {
        Self::new_with_anonymizer(
            store,
            qdrant,
            ai_embed_fn,
            batch_size,
            interval_secs,
            Arc::new(crate::training::PiiAnonymizer::disabled()),
        )
    }

    /// Construct a pipeline with an explicit anonymizer. Used
    /// by the controller-runtime when `[memory.pii] enabled =
    /// true` so the defensive pass is wired through.
    pub fn new_with_anonymizer(
        store: Arc<LayeredMemoryStore>,
        qdrant: Option<Arc<QdrantClient>>,
        ai_embed_fn: EmbedFn,
        batch_size: usize,
        interval_secs: u64,
        anonymizer: Arc<crate::training::PiiAnonymizer>,
    ) -> Self {
        Self {
            store,
            qdrant,
            ai_embed_fn,
            batch_size: batch_size.max(1),
            interval: Duration::from_secs(interval_secs.max(1)),
            anonymizer,
        }
    }

    /// Spawn the background loop. Returns the `JoinHandle` so
    /// the controller can keep it alive for the process
    /// lifetime (or drop to detach).
    ///
    /// Loop body: every `interval`, fetch up to `batch_size`
    /// records with `embedding IS NULL`. When the batch is
    /// non-empty, call `ai_embed_fn` once; on success, stamp
    /// each embedding back onto the store and (if Qdrant is
    /// wired) upsert. On `ai_embed_fn` error, log warn + sleep.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            run_loop(self).await;
        })
    }

    /// Single-tick exposed for tests: process at most
    /// `batch_size` pending records once and return the number
    /// successfully embedded. The loop body is implemented in
    /// terms of this to keep the test surface honest.
    pub async fn run_once(&self) -> usize {
        run_tick(self).await
    }
}

async fn run_loop(p: EmbeddingPipeline) {
    tracing::info!(
        batch_size = p.batch_size,
        interval_secs = p.interval.as_secs(),
        qdrant = p.qdrant.is_some(),
        "memory embedder: loop started"
    );
    loop {
        let _ = run_tick(&p).await;
        tokio::time::sleep(p.interval).await;
    }
}

async fn run_tick(p: &EmbeddingPipeline) -> usize {
    let mut pending = match p.store.fetch_pending_embeddings(p.batch_size) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "memory embedder: fetch_pending failed");
            return 0;
        }
    };
    if pending.is_empty() {
        return 0;
    }
    // RELIX-7.15 PII defense-in-depth: anonymize every
    // pending record's text BEFORE handing it to the embed
    // function. If a pending row arrived via a bypass path
    // (operator-written, migration import, etc.) we scrub it
    // here AND rewrite the on-disk row so the Qdrant payload
    // emitted below carries the anonymized text. When the
    // anonymizer is disabled (`enabled = false`) this loop is
    // a no-op.
    if p.anonymizer.enabled() {
        for r in pending.iter_mut() {
            let scrubbed = p.anonymizer.anonymize(&r.text);
            if scrubbed != r.text {
                if let Err(e) = p.store.update_text(&r.id, &scrubbed) {
                    tracing::warn!(
                        error = %e,
                        record_id = %r.id,
                        "memory embedder: pre-embed anonymize update_text failed"
                    );
                } else {
                    r.text = scrubbed;
                }
            }
        }
    }
    let texts: Vec<String> = pending.iter().map(|r| r.text.clone()).collect();
    let vectors = match (p.ai_embed_fn)(texts).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "memory embedder: ai_embed_fn failed; retrying next tick"
            );
            return 0;
        }
    };
    if vectors.len() != pending.len() {
        tracing::warn!(
            got = vectors.len(),
            want = pending.len(),
            "memory embedder: vector count mismatch; skipping batch"
        );
        return 0;
    }
    let mut embedded = 0usize;
    // GAP 23: group points by tenant so each batch lands in
    // the right per-tenant Qdrant collection. When
    // `tenant_isolation = false` the resolver returns the
    // single default collection regardless of tenant, so the
    // bucket map collapses to one key.
    let mut buckets: std::collections::HashMap<String, Vec<QdrantPoint>> =
        std::collections::HashMap::new();
    for (record, vector) in pending.iter().zip(vectors) {
        if vector.is_empty() {
            tracing::warn!(
                record_id = %record.id,
                "memory embedder: empty vector; skipping record"
            );
            continue;
        }
        match p.store.update_embedding(&record.id, vector.clone()) {
            Ok(()) => embedded += 1,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    record_id = %record.id,
                    "memory embedder: update_embedding failed; skipping record"
                );
                continue;
            }
        }
        // PART 4: collection_for_tenant now returns Result so
        // a missing tenant in multi-tenant mode is caught
        // here. The embedder pipeline skips the record + logs
        // — same posture as other Qdrant errors above; we
        // never silently route a record into a different
        // tenant's collection.
        let coll = match p.qdrant.as_ref() {
            Some(q) => match q.collection_for_tenant(record.tenant_id.as_deref()) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        record_id = %record.id,
                        "memory embedder: collection_for_tenant failed; skipping record"
                    );
                    continue;
                }
            },
            None => String::new(),
        };
        buckets
            .entry(coll)
            .or_default()
            .push(qdrant_point_for_record(record, vector));
    }
    if let Some(q) = &p.qdrant {
        for (coll, points) in buckets {
            if coll.is_empty() || points.is_empty() {
                continue;
            }
            if let Err(e) = q.upsert_in(&coll, points).await {
                tracing::warn!(
                    error = %e,
                    collection = %coll,
                    "memory embedder: qdrant upsert failed (sqlite already updated)"
                );
            }
        }
    }
    embedded
}

/// Build the Qdrant point payload from a record + its vector.
/// `text`, `source`, `tags`, `layer`, `created_at` go on the
/// payload so a downstream consumer can filter results without
/// a second SQLite round-trip.
fn qdrant_point_for_record(record: &MemoryRecord, vector: Vec<f32>) -> QdrantPoint {
    QdrantPoint {
        id: qdrant_point_id_from_str(&record.id),
        vector,
        payload: serde_json::json!({
            "id": record.id,
            "layer": record.layer.as_str(),
            "text": record.text,
            "source": record.source,
            "tags": record.tags,
            "created_at": record.created_at,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::memory::schema::{MemoryLayer, MemoryRecord};
    use std::sync::Mutex;

    fn store_with_pending(ids: &[&str]) -> Arc<LayeredMemoryStore> {
        let store = LayeredMemoryStore::in_memory().unwrap();
        for (i, id) in ids.iter().enumerate() {
            let mut r = MemoryRecord::new_raw(*id, format!("text for {id}"), "test-session");
            r.observed_at = (i as i64) * 10;
            store.insert(&r).unwrap();
        }
        Arc::new(store)
    }

    /// Embed fn that captures every call + returns canned
    /// vectors (one per text, deterministic).
    fn recording_embed(captured: Arc<Mutex<Vec<Vec<String>>>>, per_text_dim: usize) -> EmbedFn {
        Arc::new(move |texts: Vec<String>| {
            let captured = captured.clone();
            Box::pin(async move {
                captured.lock().unwrap().push(texts.clone());
                let out: Vec<Vec<f32>> = texts
                    .iter()
                    .enumerate()
                    .map(|(i, _)| {
                        let mut v = vec![0.0f32; per_text_dim];
                        if per_text_dim > 0 {
                            v[0] = (i as f32) + 1.0;
                        }
                        v
                    })
                    .collect();
                Ok(out)
            }) as BoxFuture<'static, Result<Vec<Vec<f32>>, String>>
        })
    }

    fn failing_embed() -> EmbedFn {
        Arc::new(|_texts: Vec<String>| {
            Box::pin(async move { Err("ai.embed unreachable".to_string()) })
                as BoxFuture<'static, Result<Vec<Vec<f32>>, String>>
        })
    }

    #[tokio::test]
    async fn pipeline_calls_ai_embed_fn_with_pending_record_texts() {
        let store = store_with_pending(&["a", "b"]);
        let captured: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let embed = recording_embed(captured.clone(), 4);
        let pipeline = EmbeddingPipeline::new(store.clone(), None, embed, 32, 60);
        let n = pipeline.run_once().await;
        assert_eq!(n, 2, "two pending records should be embedded");
        let calls = captured.lock().unwrap();
        assert_eq!(calls.len(), 1, "one batched call");
        assert_eq!(calls[0], vec!["text for a", "text for b"]);
    }

    #[tokio::test]
    async fn successfully_embedded_records_persist_in_store() {
        let store = store_with_pending(&["a"]);
        let captured: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let embed = recording_embed(captured, 4);
        let pipeline = EmbeddingPipeline::new(store.clone(), None, embed, 8, 60);
        let n = pipeline.run_once().await;
        assert_eq!(n, 1);
        let got = store.get("a").unwrap().unwrap();
        let v = got.embedding.expect("embedding stamped");
        assert_eq!(v.len(), 4);
        // Second tick: no pending → no work + no panic.
        let n2 = pipeline.run_once().await;
        assert_eq!(n2, 0);
    }

    #[tokio::test]
    async fn failed_embed_fn_skips_records_without_blowing_up() {
        let store = store_with_pending(&["a", "b"]);
        let pipeline = EmbeddingPipeline::new(store.clone(), None, failing_embed(), 8, 60);
        let n = pipeline.run_once().await;
        assert_eq!(n, 0, "no records embedded when embed fn errors");
        // Records remain pending, ready for the next tick.
        let counts = store.count_pending_embeddings().unwrap();
        assert_eq!(counts.get(&MemoryLayer::Raw).copied().unwrap_or(0), 2);
    }

    #[tokio::test]
    async fn qdrant_upsert_is_called_with_record_payload_shape() {
        // Spin a tiny axum mock that captures the upsert body.
        use axum::Router;
        use axum::routing::any;
        let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = captured.clone();
        let app = Router::new().fallback(any(move |req: axum::http::Request<axum::body::Body>| {
            let captured = captured_clone.clone();
            async move {
                let path = req.uri().path().to_string();
                let bytes = axum::body::to_bytes(req.into_body(), 64 * 1024)
                    .await
                    .unwrap_or_default();
                let body: serde_json::Value = if bytes.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
                };
                if path.contains("/points") {
                    captured.lock().unwrap().push(body);
                }
                axum::Json(serde_json::json!({
                    "result": true,
                    "status": "ok",
                    "time": 0.001,
                }))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let qcfg = crate::nodes::memory::qdrant::QdrantConfig {
            url: format!("http://{addr}"),
            collection: "t".into(),
            dim: 4,
            api_key: None,
            tenant_isolation: false,
            collection_prefix: "relix".into(),
        };
        let qclient = Arc::new(QdrantClient::new(qcfg));
        let store = store_with_pending(&["rec-1"]);
        let cap2: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let pipeline =
            EmbeddingPipeline::new(store, Some(qclient), recording_embed(cap2, 4), 8, 60);
        let n = pipeline.run_once().await;
        assert_eq!(n, 1);
        let calls = captured.lock().unwrap();
        assert!(
            calls.iter().any(|b| b.get("points").is_some()),
            "upsert payload should carry a points array; got: {calls:?}"
        );
        let upsert = calls
            .iter()
            .find(|b| b.get("points").is_some())
            .unwrap()
            .clone();
        let pt = &upsert["points"][0];
        assert_eq!(pt["payload"]["id"], "rec-1");
        assert_eq!(pt["payload"]["layer"], "raw");
        assert!(pt["vector"].is_array());
        let dim = pt["vector"].as_array().unwrap().len();
        assert_eq!(dim, 4);
    }

    #[tokio::test]
    async fn vector_count_mismatch_skips_batch_without_panicking() {
        // Embed fn returns 0 vectors despite 2 inputs — pipeline
        // must log + bail without partial writes.
        let store = store_with_pending(&["a", "b"]);
        let bad_embed: EmbedFn = Arc::new(|_texts: Vec<String>| {
            Box::pin(async move { Ok(Vec::<Vec<f32>>::new()) })
                as BoxFuture<'static, Result<Vec<Vec<f32>>, String>>
        });
        let pipeline = EmbeddingPipeline::new(store.clone(), None, bad_embed, 8, 60);
        let n = pipeline.run_once().await;
        assert_eq!(n, 0);
        let a = store.get("a").unwrap().unwrap();
        let b = store.get("b").unwrap().unwrap();
        assert!(a.embedding.is_none());
        assert!(b.embedding.is_none());
    }

    #[test]
    fn embedder_config_deserializes_with_defaults() {
        let cfg: EmbedderConfig = toml::from_str("enabled = true").unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.batch_size, 32);
        assert_eq!(cfg.interval_secs, 60);
        assert!((cfg.score_threshold - 0.75).abs() < 1e-6);
    }

    #[test]
    fn embedder_config_default_is_disabled() {
        let cfg = EmbedderConfig::default();
        assert!(!cfg.enabled);
    }
}
