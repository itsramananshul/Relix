//! Async metrics collector for RELIX-7.11.
//!
//! Wraps a [`super::store::MetricsStore`] and exposes a
//! non-blocking, `Send + Sync` recording surface to the
//! dispatch bridge.
//!
//! ## Hot path
//!
//! `record_invocation` is called from inside
//! `DispatchBridge::handle_inbound` after every dispatched
//! capability. It must NEVER block, NEVER fsync, NEVER take a
//! contended lock. The implementation:
//!
//! 1. Looks up the request id in a small in-memory join cache
//!    (mutex-guarded; the lock is held for microseconds).
//! 2. Merges any matching `AiUsageHint` into the metric (sync,
//!    pure CPU).
//! 3. Sends the enriched metric down an `unbounded` mpsc
//!    channel — never blocks.
//!
//! ## Drain task
//!
//! A background task owns the receiver side, batches up to 100
//! rows or up to 100ms (whichever comes first), and writes the
//! batch as one transaction.
//!
//! ## Retention loop
//!
//! A second background task runs every hour and deletes rows
//! older than `retention_days * 86_400_000` ms.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use relix_core::types::RequestId;

use super::pricing::PriceTable;
use super::store::MetricsStore;
#[cfg(test)]
use super::store::MetricsStoreError;
use super::types::{AiProviderSignalsHint, AiUsageHint, InvocationMetric};

/// Trait the dispatch bridge holds. Stripped down so the
/// dispatch tests can stub it without pulling in sqlite.
pub trait MetricsSink: Send + Sync {
    fn record_invocation(&self, m: InvocationMetric);
    fn attach_ai_usage(&self, hint: AiUsageHint);
    /// RELIX-7.19 GAP 3: attach a provider-signals hint
    /// (finish_reason + logprob) keyed by `request_id`. The
    /// dispatch bridge calls [`Self::take_provider_signals`]
    /// during confidence scoring to retrieve the matching
    /// hint. Default no-op for back-compat with sinks that
    /// don't care about confidence scoring.
    fn attach_provider_signals(&self, _hint: AiProviderSignalsHint) {}
    /// RELIX-7.19 GAP 3: pop the provider-signals hint
    /// matching `request_id`, if any. Default returns `None`.
    fn take_provider_signals(&self, _request_id: RequestId) -> Option<AiProviderSignalsHint> {
        None
    }
}

/// Production sink. Cheap to clone (couple of `Arc`s).
#[derive(Clone)]
pub struct MetricsCollector {
    tx: mpsc::UnboundedSender<InvocationMetric>,
    hints: Arc<Mutex<HashMap<RequestId, AiUsageHint>>>,
    /// RELIX-7.19 GAP 3: per-request provider-signals join
    /// cache. Bounded by [`HINT_CACHE_CAP`]; FIFO-cleared on
    /// overflow same as [`MetricsCollector::hints`].
    provider_signals: Arc<Mutex<HashMap<RequestId, AiProviderSignalsHint>>>,
    prices: Arc<PriceTable>,
    store: MetricsStore,
    /// RELIX-7.28 Part 1: optional budget enforcer the collector
    /// invalidates whenever a cost-bearing metric lands. The
    /// enforcer's in-memory cache is otherwise refreshed every
    /// 60s; immediate invalidation closes that gap so a single
    /// expensive call cannot escape a same-window cap by being
    /// the last call before a check.
    budget: Arc<Mutex<Option<Arc<super::budget::BudgetEnforcer>>>>,
}

/// How many pending AI usage hints we hold in memory while
/// waiting for their matching dispatch record. Sized to absorb
/// a burst from a parallel-fanned-out workflow without unbounded
/// growth. Hints not consumed within this window are evicted
/// FIFO on insertion.
pub const HINT_CACHE_CAP: usize = 4096;

/// How long the drain task waits for the batch to fill before
/// flushing what it has.
pub const BATCH_INTERVAL_MS: u64 = 100;

/// Maximum number of metrics flushed in one transaction.
pub const BATCH_SIZE: usize = 100;

impl MetricsCollector {
    /// Build a collector around the given `store` + price
    /// table. The drain + retention tasks are spawned via
    /// [`MetricsCollector::spawn_workers`].
    pub fn new(store: MetricsStore, prices: PriceTable) -> (Self, MetricsWorkerHandles) {
        let (tx, rx) = mpsc::unbounded_channel();
        let collector = Self {
            tx,
            hints: Arc::new(Mutex::new(HashMap::with_capacity(HINT_CACHE_CAP))),
            provider_signals: Arc::new(Mutex::new(HashMap::with_capacity(HINT_CACHE_CAP))),
            prices: Arc::new(prices),
            store: store.clone(),
            budget: Arc::new(Mutex::new(None)),
        };
        let handles = MetricsWorkerHandles {
            store,
            receiver: Some(rx),
        };
        (collector, handles)
    }

    /// Cheap-clone handle to the price table — used by handlers
    /// that want to estimate cost before the metric is written
    /// (e.g. quota guards). The collector keeps its own clone.
    pub fn prices(&self) -> Arc<PriceTable> {
        self.prices.clone()
    }

    /// Cheap-clone handle to the store — used by the query
    /// engine + retention loop.
    pub fn store(&self) -> MetricsStore {
        self.store.clone()
    }

    /// Synchronously merge a pending AI usage hint into a
    /// metric — pulled out for testing.
    pub(crate) fn enrich_inline(&self, m: &mut InvocationMetric) {
        if let Some(req_id) = m.request_id
            && let Some(hint) = take_hint(&self.hints, &req_id)
        {
            m.enrich_with_hint(&hint, &self.prices);
        }
    }

    /// RELIX-7.28 Part 1: wire the budget enforcer so cost-bearing
    /// metrics force-invalidate the enforcer's cache for the
    /// agent (and the deployment-level cache, since deployment
    /// totals reflect every agent's spend).
    pub fn set_budget_enforcer(&self, enforcer: Arc<super::budget::BudgetEnforcer>) {
        let mut g = match self.budget.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *g = Some(enforcer);
    }
}

impl MetricsSink for MetricsCollector {
    fn record_invocation(&self, mut m: InvocationMetric) {
        self.enrich_inline(&mut m);
        // RELIX-7.28 Part 1: invalidate the BudgetEnforcer's cache
        // immediately when this row contributes spend. The cache's
        // 60-second refresh tick is otherwise the upper bound on
        // how stale the in-memory accumulated cost can be.
        if let Some(cost) = m.cost_micros
            && cost > 0
        {
            let enforcer = match self.budget.lock() {
                Ok(g) => g.clone(),
                Err(p) => p.into_inner().clone(),
            };
            if let Some(e) = enforcer {
                e.invalidate_agent(&m.agent_name);
            }
        }
        match self.tx.send(m) {
            Ok(()) => {}
            Err(_) => {
                // Receiver dropped — drain task is gone. We
                // never want to panic on the hot path. The
                // first error is loud (warn); subsequent
                // attempts silently drop because the controller
                // will be tearing down anyway.
                tracing::warn!(
                    "metrics: drain task receiver dropped; further metrics will be silently lost"
                );
            }
        }
    }

    fn attach_ai_usage(&self, hint: AiUsageHint) {
        let mut g = match self.hints.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(), // recover from poisoning — losing one row is fine
        };
        if g.len() >= HINT_CACHE_CAP {
            // FIFO eviction is approximated by simply clearing
            // the cache when it overflows. The hint cache is a
            // best-effort enrichment path; dropping in bursts
            // is preferable to unbounded growth.
            g.clear();
        }
        g.insert(hint.request_id, hint);
    }

    fn attach_provider_signals(&self, hint: AiProviderSignalsHint) {
        let mut g = match self.provider_signals.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if g.len() >= HINT_CACHE_CAP {
            g.clear();
        }
        g.insert(hint.request_id, hint);
    }

    fn take_provider_signals(&self, request_id: RequestId) -> Option<AiProviderSignalsHint> {
        let mut g = match self.provider_signals.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.remove(&request_id)
    }
}

fn take_hint(
    hints: &Arc<Mutex<HashMap<RequestId, AiUsageHint>>>,
    req_id: &RequestId,
) -> Option<AiUsageHint> {
    let mut g = match hints.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    g.remove(req_id)
}

/// Owned worker handles returned by [`MetricsCollector::new`].
/// Call [`spawn`](Self::spawn) once on startup to start the
/// drain + retention loops. Drops cleanly on shutdown — the
/// drain loop exits when the collector's sender is dropped.
pub struct MetricsWorkerHandles {
    store: MetricsStore,
    receiver: Option<mpsc::UnboundedReceiver<InvocationMetric>>,
}

/// Retention configuration handed to the worker.
#[derive(Clone, Debug)]
pub struct RetentionConfig {
    /// Days to keep metric rows. Rows older than `now -
    /// retention_days * 86400_000 ms` are deleted hourly.
    pub retention_days: u32,
    /// Interval between retention sweeps. Tests override to a
    /// short value; production defaults to 1h.
    pub sweep_interval: Duration,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            retention_days: 30,
            sweep_interval: Duration::from_secs(3600),
        }
    }
}

impl MetricsWorkerHandles {
    /// Spawn the drain loop + the retention loop on the
    /// current tokio runtime. Returns `JoinHandle`s purely
    /// for tests; production code drops them.
    pub fn spawn(self, retention: RetentionConfig) -> SpawnedMetrics {
        let rx = self
            .receiver
            .expect("MetricsWorkerHandles::spawn called twice");
        let drain_store = self.store.clone();
        let retention_store = self.store.clone();
        let drain = tokio::spawn(async move {
            run_drain_loop(rx, drain_store).await;
        });
        let retention_task = tokio::spawn(async move {
            run_retention_loop(retention_store, retention).await;
        });
        SpawnedMetrics {
            drain,
            retention: retention_task,
        }
    }
}

/// Handles returned by [`MetricsWorkerHandles::spawn`].
pub struct SpawnedMetrics {
    pub drain: tokio::task::JoinHandle<()>,
    pub retention: tokio::task::JoinHandle<()>,
}

async fn run_drain_loop(mut rx: mpsc::UnboundedReceiver<InvocationMetric>, store: MetricsStore) {
    let mut batch: Vec<InvocationMetric> = Vec::with_capacity(BATCH_SIZE);
    let mut tick = tokio::time::interval(Duration::from_millis(BATCH_INTERVAL_MS));
    // Skip the first immediate tick — interval fires at t=0.
    tick.tick().await;
    loop {
        tokio::select! {
            biased;
            recv = rx.recv() => {
                match recv {
                    Some(m) => {
                        batch.push(m);
                        if batch.len() >= BATCH_SIZE {
                            flush_batch(&store, &mut batch);
                        }
                    }
                    None => {
                        // Sender dropped — flush remaining and exit.
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

fn flush_batch(store: &MetricsStore, batch: &mut Vec<InvocationMetric>) {
    if batch.is_empty() {
        return;
    }
    if let Err(e) = store.insert_batch(batch) {
        tracing::warn!(error = %e, rows = batch.len(), "metrics: batch insert failed");
    }
    batch.clear();
}

async fn run_retention_loop(store: MetricsStore, cfg: RetentionConfig) {
    let mut tick = tokio::time::interval(cfg.sweep_interval);
    // Skip first immediate tick.
    tick.tick().await;
    loop {
        tick.tick().await;
        let cutoff_ms = now_ms() - (cfg.retention_days as i64) * 86_400_000;
        match store.prune_older_than(cutoff_ms) {
            Ok(0) => {
                tracing::debug!("metrics retention: no rows past cutoff");
            }
            Ok(n) => {
                tracing::info!(deleted = n, "metrics retention: pruned old rows");
            }
            Err(e) => {
                tracing::warn!(error = %e, "metrics retention: prune failed");
            }
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Used only by tests — flush any pending metric synchronously.
/// Drains the channel until empty.
#[cfg(test)]
pub fn flush_for_test(
    rx: &mut mpsc::UnboundedReceiver<InvocationMetric>,
    store: &MetricsStore,
) -> Result<usize, MetricsStoreError> {
    let mut batch = Vec::new();
    while let Ok(m) = rx.try_recv() {
        batch.push(m);
    }
    let n = batch.len();
    store.insert_batch(&batch)?;
    Ok(n)
}

/// Convenience: a no-op sink for handlers that compile with a
/// metrics-disabled bridge. Used by tests that don't care.
#[derive(Clone, Default)]
pub struct NullMetricsSink;

impl MetricsSink for NullMetricsSink {
    fn record_invocation(&self, _: InvocationMetric) {}
    fn attach_ai_usage(&self, _: AiUsageHint) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::types::RequestId;

    fn rid(seed: u8) -> RequestId {
        RequestId([seed; 16])
    }

    fn metric(req: RequestId, agent: &str, method: &str, ts: i64) -> InvocationMetric {
        InvocationMetric {
            agent_name: agent.into(),
            peer_alias: "coord".into(),
            method: method.into(),
            timestamp_ms: ts,
            latency_ms: 12,
            success: true,
            error_kind: None,
            token_count: None,
            cost_micros: None,
            input_bytes: 16,
            output_bytes: 32,
            model: None,
            confidence_score: None,
            routing_tier: None,
            request_id: Some(req),
        }
    }

    #[tokio::test]
    async fn record_invocation_writes_through_drain_loop() {
        let store = MetricsStore::in_memory().unwrap();
        let prices = PriceTable::with_defaults();
        let (col, handles) = MetricsCollector::new(store.clone(), prices);
        let _spawned = handles.spawn(RetentionConfig {
            retention_days: 30,
            sweep_interval: Duration::from_secs(3600),
        });
        col.record_invocation(metric(rid(1), "alice", "ai.chat", 100));
        // Allow drain loop to wake up.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(store.row_count().unwrap(), 1);
        drop(col);
        // Drop the collector to close the channel — drain
        // loop should exit on its own.
    }

    #[tokio::test]
    async fn batch_flushes_when_size_reached() {
        let store = MetricsStore::in_memory().unwrap();
        let prices = PriceTable::with_defaults();
        let (col, handles) = MetricsCollector::new(store.clone(), prices);
        let _spawned = handles.spawn(RetentionConfig {
            retention_days: 30,
            sweep_interval: Duration::from_secs(3600),
        });
        for i in 0..BATCH_SIZE {
            col.record_invocation(metric(rid(i as u8), "alice", "ai.chat", 100 + i as i64));
        }
        // The 100-row batch should flush before the 100ms
        // interval elapses. Give the runtime a brief slice to
        // notice.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(store.row_count().unwrap() as usize, BATCH_SIZE);
    }

    #[tokio::test]
    async fn batch_flushes_on_interval_when_under_size() {
        let store = MetricsStore::in_memory().unwrap();
        let prices = PriceTable::with_defaults();
        let (col, handles) = MetricsCollector::new(store.clone(), prices);
        let _spawned = handles.spawn(RetentionConfig {
            retention_days: 30,
            sweep_interval: Duration::from_secs(3600),
        });
        // Insert ten rows — way under BATCH_SIZE — and let the
        // 100ms timer tick.
        for i in 0..10 {
            col.record_invocation(metric(rid(i), "alice", "ai.chat", 100 + i as i64));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(store.row_count().unwrap(), 10);
    }

    #[tokio::test]
    async fn ai_usage_hint_enriches_subsequent_metric() {
        let store = MetricsStore::in_memory().unwrap();
        let prices = PriceTable::with_defaults();
        let (col, handles) = MetricsCollector::new(store.clone(), prices);
        let _spawned = handles.spawn(RetentionConfig::default());
        let req = rid(99);
        col.attach_ai_usage(AiUsageHint {
            request_id: req,
            prompt_tokens: 100,
            completion_tokens: 200,
            model: "gpt-4o-mini".into(),
            routing_tier: None,
        });
        col.record_invocation(metric(req, "alice", "ai.chat", 100));
        tokio::time::sleep(Duration::from_millis(250)).await;
        let (tokens, cost, model): (Option<i64>, Option<i64>, Option<String>) = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT token_count, cost_micros, model FROM metrics_invocations",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
            })
            .unwrap();
        assert_eq!(tokens, Some(300));
        assert!(cost.unwrap() > 0);
        assert_eq!(model.as_deref(), Some("gpt-4o-mini"));
    }

    #[tokio::test]
    async fn retention_cleans_rows_outside_window() {
        let store = MetricsStore::in_memory().unwrap();
        // Pre-populate with an old + new row.
        let mut m_old = metric(rid(1), "alice", "ai.chat", 100);
        m_old.timestamp_ms = 0; // ancient
        store.insert(&m_old).unwrap();
        let mut m_new = metric(rid(2), "alice", "ai.chat", 100);
        m_new.timestamp_ms = now_ms();
        store.insert(&m_new).unwrap();
        let prices = PriceTable::with_defaults();
        let (_col, handles) = MetricsCollector::new(store.clone(), prices);
        // Fast sweep interval so the test doesn't wait an hour.
        let _spawned = handles.spawn(RetentionConfig {
            retention_days: 1,
            sweep_interval: Duration::from_millis(100),
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(store.row_count().unwrap(), 1);
    }

    #[tokio::test]
    async fn retention_keeps_rows_inside_window() {
        let store = MetricsStore::in_memory().unwrap();
        // Insert a row at exactly "now" — well within any sane
        // retention window.
        let mut m_new = metric(rid(1), "alice", "ai.chat", 100);
        m_new.timestamp_ms = now_ms();
        store.insert(&m_new).unwrap();
        let prices = PriceTable::with_defaults();
        let (_col, handles) = MetricsCollector::new(store.clone(), prices);
        let _spawned = handles.spawn(RetentionConfig {
            retention_days: 30,
            sweep_interval: Duration::from_millis(50),
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(store.row_count().unwrap(), 1);
    }

    #[test]
    fn null_sink_accepts_metrics_and_hints_without_panicking() {
        let sink = NullMetricsSink;
        sink.record_invocation(metric(rid(1), "alice", "ai.chat", 100));
        sink.attach_ai_usage(AiUsageHint {
            request_id: rid(1),
            prompt_tokens: 10,
            completion_tokens: 20,
            model: "mock".into(),
            routing_tier: None,
        });
    }

    #[test]
    fn hint_cache_overflow_clears_and_continues() {
        let store = MetricsStore::in_memory().unwrap();
        let prices = PriceTable::with_defaults();
        let (col, _h) = MetricsCollector::new(store, prices);
        // Use a unique 16-byte RequestId per insert by encoding
        // the loop index into the byte array, so we actually
        // exercise the overflow path (a u8-seeded key wraps and
        // overwrites at 256).
        fn unique_rid(i: usize) -> RequestId {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&(i as u64).to_le_bytes());
            RequestId(b)
        }
        for i in 0..(HINT_CACHE_CAP + 10) {
            col.attach_ai_usage(AiUsageHint {
                request_id: unique_rid(i),
                prompt_tokens: 1,
                completion_tokens: 1,
                model: "mock".into(),
                routing_tier: None,
            });
        }
        // After overflow + 10 more inserts the cache should
        // hold at most the 10 post-clear entries, proving the
        // overflow guard ran.
        let g = col.hints.lock().unwrap();
        assert!(
            g.len() <= 10,
            "expected ≤10 after overflow, got {}",
            g.len()
        );
    }
}
