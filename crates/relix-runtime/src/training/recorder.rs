//! RELIX-7.15 — non-blocking InteractionRecorder + retention loop.
//!
//! Hot path: the AI handler calls
//! [`InteractionSink::record_interaction`] after every
//! `ai.chat` / `ai.chat.stream` turn. The call MUST NOT block,
//! MUST NOT fsync, MUST NOT hold a contended lock. The
//! implementation:
//!
//! 1. Stamps `recorded_at` with the current wall clock if the
//!    caller passed `0`.
//! 2. Sends the record down an unbounded mpsc channel.
//!
//! Drain task: owns the receiver, batches up to 100 rows or
//! 100ms (whichever comes first) and writes the batch in one
//! transaction.
//!
//! Retention task: runs every
//! `retention_sweep_interval_secs` and deletes rows older than
//! `retention_days * 86_400_000` ms.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::store::TrainingStore;
use super::types::InteractionRecord;

/// Trait the AI handler holds. Stripped down so non-recording
/// builds can use [`NullInteractionSink`] without dragging in
/// SQLite.
pub trait InteractionSink: Send + Sync {
    fn record_interaction(&self, rec: InteractionRecord);
}

/// Production sink — non-blocking mpsc producer in front of the
/// shared drain task. Cheap to clone.
#[derive(Clone)]
pub struct InteractionRecorder {
    tx: mpsc::UnboundedSender<InteractionRecord>,
    store: TrainingStore,
}

pub const BATCH_INTERVAL_MS: u64 = 100;
pub const BATCH_SIZE: usize = 100;

impl InteractionRecorder {
    pub fn new(store: TrainingStore) -> (Self, RecorderWorkerHandles) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                store: store.clone(),
            },
            RecorderWorkerHandles {
                store,
                receiver: Some(rx),
            },
        )
    }

    pub fn store(&self) -> TrainingStore {
        self.store.clone()
    }
}

impl InteractionSink for InteractionRecorder {
    fn record_interaction(&self, mut rec: InteractionRecord) {
        if rec.recorded_at == 0 {
            rec.recorded_at = now_ms();
        }
        if self.tx.send(rec).is_err() {
            tracing::warn!(
                "training: drain task receiver dropped; further interactions will be silently lost"
            );
        }
    }
}

/// Owned worker handles returned by
/// [`InteractionRecorder::new`]. Call
/// [`Self::spawn`](Self::spawn) once to start the drain +
/// retention loops.
pub struct RecorderWorkerHandles {
    store: TrainingStore,
    receiver: Option<mpsc::UnboundedReceiver<InteractionRecord>>,
}

#[derive(Clone, Debug)]
pub struct RetentionConfig {
    pub retention_days: u32,
    pub sweep_interval: Duration,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            retention_days: 90,
            sweep_interval: Duration::from_secs(86_400),
        }
    }
}

pub struct SpawnedRecorder {
    pub drain: tokio::task::JoinHandle<()>,
    pub retention: tokio::task::JoinHandle<()>,
}

impl RecorderWorkerHandles {
    pub fn spawn(self, retention: RetentionConfig) -> SpawnedRecorder {
        let rx = self
            .receiver
            .expect("RecorderWorkerHandles::spawn called twice");
        let drain_store = self.store.clone();
        let retention_store = self.store.clone();
        let drain = tokio::spawn(async move {
            run_drain_loop(rx, drain_store).await;
        });
        let retention_task = tokio::spawn(async move {
            run_retention_loop(retention_store, retention).await;
        });
        SpawnedRecorder {
            drain,
            retention: retention_task,
        }
    }
}

async fn run_drain_loop(mut rx: mpsc::UnboundedReceiver<InteractionRecord>, store: TrainingStore) {
    let mut batch: Vec<InteractionRecord> = Vec::with_capacity(BATCH_SIZE);
    let mut tick = tokio::time::interval(Duration::from_millis(BATCH_INTERVAL_MS));
    tick.tick().await;
    loop {
        tokio::select! {
            biased;
            recv = rx.recv() => {
                match recv {
                    Some(rec) => {
                        batch.push(rec);
                        if batch.len() >= BATCH_SIZE {
                            flush_batch(&store, &mut batch);
                        }
                    }
                    None => {
                        flush_batch(&store, &mut batch);
                        return;
                    }
                }
            }
            _ = tick.tick() => {
                if !batch.is_empty() {
                    flush_batch(&store, &mut batch);
                }
            }
        }
    }
}

fn flush_batch(store: &TrainingStore, batch: &mut Vec<InteractionRecord>) {
    if batch.is_empty() {
        return;
    }
    if let Err(e) = store.insert_batch(batch) {
        tracing::warn!(error = %e, rows = batch.len(), "training: batch insert failed");
    }
    batch.clear();
}

async fn run_retention_loop(store: TrainingStore, cfg: RetentionConfig) {
    let mut tick = tokio::time::interval(cfg.sweep_interval);
    tick.tick().await;
    loop {
        tick.tick().await;
        let cutoff_ms = now_ms() - (cfg.retention_days as i64) * 86_400_000;
        match store.prune_older_than(cutoff_ms) {
            Ok(0) => tracing::debug!("training retention: no rows past cutoff"),
            Ok(n) => tracing::info!(deleted = n, "training retention: pruned old rows"),
            Err(e) => tracing::warn!(error = %e, "training retention: prune failed"),
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// No-op sink used by callers that have not enabled the
/// `[training]` section. Pre-bound to an `Arc<dyn
/// InteractionSink>` for convenience.
#[derive(Clone, Default)]
pub struct NullInteractionSink;

impl InteractionSink for NullInteractionSink {
    fn record_interaction(&self, _: InteractionRecord) {}
}

/// Convenience: a sink that records every received interaction
/// into an `Arc<Mutex<Vec<...>>>`. Used by integration tests that
/// don't want to spin up the drain loop.
#[derive(Clone, Default)]
pub struct CollectingInteractionSink {
    pub log: Arc<std::sync::Mutex<Vec<InteractionRecord>>>,
}

impl InteractionSink for CollectingInteractionSink {
    fn record_interaction(&self, rec: InteractionRecord) {
        let mut g = match self.log.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.push(rec);
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{InteractionId, InteractionRecord};
    use super::*;

    fn record(id: &str, agent: &str, ts: i64) -> InteractionRecord {
        InteractionRecord {
            interaction_id: InteractionId(id.into()),
            session_id: "s".into(),
            agent: agent.into(),
            model: "gpt-4o-mini".into(),
            provider: "openai".into(),
            system_prompt: String::new(),
            user_message: "hi".into(),
            response: "hello".into(),
            tool_calls: vec![],
            token_count: Some(10),
            prompt_tokens: Some(4),
            completion_tokens: Some(6),
            latency_ms: 100,
            success: true,
            error_kind: None,
            recorded_at: ts,
            quality_score: None,
            exported: false,
            export_set: None,
        }
    }

    #[tokio::test]
    async fn record_persists_through_drain_loop() {
        let store = TrainingStore::in_memory().unwrap();
        let (rec, handles) = InteractionRecorder::new(store.clone());
        let _h = handles.spawn(RetentionConfig::default());
        rec.record_interaction(record("a1", "alice", 100));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(store.row_count().unwrap(), 1);
        let got = store.get("a1").unwrap().unwrap();
        assert_eq!(got.interaction_id.as_str(), "a1");
        drop(rec);
    }

    #[tokio::test]
    async fn record_stamps_recorded_at_when_zero() {
        let store = TrainingStore::in_memory().unwrap();
        let (rec, handles) = InteractionRecorder::new(store.clone());
        let _h = handles.spawn(RetentionConfig::default());
        let before = now_ms();
        rec.record_interaction(record("z", "alice", 0));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let got = store.get("z").unwrap().unwrap();
        assert!(got.recorded_at >= before);
        drop(rec);
    }

    #[tokio::test]
    async fn batch_flushes_at_size_threshold() {
        let store = TrainingStore::in_memory().unwrap();
        let (rec, handles) = InteractionRecorder::new(store.clone());
        let _h = handles.spawn(RetentionConfig::default());
        for i in 0..BATCH_SIZE {
            rec.record_interaction(record(&format!("id{i:03}"), "alice", 100 + i as i64));
        }
        // Size-based flush should happen well before the 100ms timer.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(store.row_count().unwrap() as usize, BATCH_SIZE);
        drop(rec);
    }

    #[tokio::test]
    async fn retention_deletes_only_rows_outside_window() {
        let store = TrainingStore::in_memory().unwrap();
        let mut old = record("old", "alice", 100);
        old.recorded_at = 0;
        store.insert(&old).unwrap();
        let mut newer = record("new", "alice", 100);
        newer.recorded_at = now_ms();
        store.insert(&newer).unwrap();
        let (_rec, handles) = InteractionRecorder::new(store.clone());
        let _h = handles.spawn(RetentionConfig {
            retention_days: 1,
            sweep_interval: Duration::from_millis(50),
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(store.row_count().unwrap(), 1);
        assert!(store.get("new").unwrap().is_some());
        assert!(store.get("old").unwrap().is_none());
    }

    #[tokio::test]
    async fn retention_keeps_rows_within_window() {
        let store = TrainingStore::in_memory().unwrap();
        let mut newer = record("new", "alice", 100);
        newer.recorded_at = now_ms();
        store.insert(&newer).unwrap();
        let (_rec, handles) = InteractionRecorder::new(store.clone());
        let _h = handles.spawn(RetentionConfig {
            retention_days: 30,
            sweep_interval: Duration::from_millis(50),
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(store.row_count().unwrap(), 1);
    }

    #[test]
    fn null_sink_accepts_records_without_panicking() {
        let s = NullInteractionSink;
        s.record_interaction(record("x", "alice", 100));
    }

    #[test]
    fn collecting_sink_captures_records_in_order() {
        let s = CollectingInteractionSink::default();
        s.record_interaction(record("a", "alice", 100));
        s.record_interaction(record("b", "alice", 200));
        let g = s.log.lock().unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].interaction_id.as_str(), "a");
        assert_eq!(g[1].interaction_id.as_str(), "b");
    }
}
