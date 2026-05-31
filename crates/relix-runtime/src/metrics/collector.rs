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

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lru::LruCache;
use relix_core::types::RequestId;

use super::pricing::PriceTable;
use super::store::MetricsStore;
#[cfg(test)]
use super::store::MetricsStoreError;
use super::types::{AiProviderSignalsHint, AiSelfConsistencyHint, AiUsageHint, InvocationMetric};

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
    /// RELIX-7.29 PART 2: attach the self-consistency score
    /// hint emitted by the AI handler when adaptive SC
    /// sampling has been run for this `request_id`. The
    /// dispatch bridge's `ConfidenceScorer` pops it during
    /// scoring and REPLACES the `provider_signal` sub-score
    /// with `hint.score`. Default no-op.
    fn attach_self_consistency(&self, _hint: AiSelfConsistencyHint) {}
    /// RELIX-7.29 PART 2: pop the self-consistency hint
    /// matching `request_id`. Default returns `None`.
    fn take_self_consistency(&self, _request_id: RequestId) -> Option<AiSelfConsistencyHint> {
        None
    }
}

/// Production sink. Cheap to clone (couple of `Arc`s).
#[derive(Clone)]
pub struct MetricsCollector {
    /// CORR PART 4: bounded drop-oldest channel replaces the
    /// pre-fix `mpsc::UnboundedSender`. A stuck drain task
    /// used to let the queue grow without bound; the bounded
    /// channel evicts the oldest entry at the [`METRICS_CHANNEL_CAP`]
    /// boundary so the latest signal is always preserved.
    channel: BoundedDropOldestChannel<InvocationMetric>,
    /// PART 5: drop-oldest LRU. The pre-fix HashMap path
    /// cleared the WHOLE cache when it overflowed, which
    /// dropped every in-flight hint at once. The LRU evicts
    /// the single oldest entry per insertion, bumps
    /// `dropped_hints`, and warns at most once per minute via
    /// `last_warn_ms`.
    hints: Arc<Mutex<LruCache<RequestId, AiUsageHint>>>,
    /// PART 5: same drop-oldest discipline as `hints`.
    provider_signals: Arc<Mutex<LruCache<RequestId, AiProviderSignalsHint>>>,
    /// PART 5: same drop-oldest discipline as `hints`.
    self_consistency: Arc<Mutex<LruCache<RequestId, AiSelfConsistencyHint>>>,
    /// PART 5: lifetime count of hints evicted from the
    /// drop-oldest LRU caches above. Reported via
    /// [`Self::dropped_hints`] for operator dashboards.
    dropped_hints: Arc<AtomicU64>,
    /// PART 5: last-warning timestamp (ms-since-epoch) for the
    /// hint-cache overflow warn. Rate-limited to once per
    /// minute so a sustained overflow doesn't flood logs.
    last_warn_ms: Arc<AtomicI64>,
    prices: Arc<PriceTable>,
    store: MetricsStore,
    /// RELIX-7.28 Part 1: optional budget enforcer the collector
    /// invalidates whenever a cost-bearing metric lands. The
    /// enforcer's in-memory cache is otherwise refreshed every
    /// 60s; immediate invalidation closes that gap so a single
    /// expensive call cannot escape a same-window cap by being
    /// the last call before a check.
    budget: Arc<Mutex<Option<Arc<super::budget::BudgetEnforcer>>>>,
    /// PART 4: absolute spend caps state. Installed once by the
    /// controller wiring via [`Self::install_absolute_caps`].
    /// When absent the per-request / hourly / daily checks are
    /// silently inert.
    absolute_caps: Arc<std::sync::OnceLock<AbsoluteCapsHandle>>,
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

/// CORR PART 4: hard cap on the in-flight metrics queue. The
/// pre-fix path used `mpsc::unbounded_channel` so a stuck drain
/// task (DB locked, fsync stall) let the queue grow without
/// limit. The bounded queue drops the OLDEST entry once the
/// cap is hit so the latest signal is always preserved and the
/// dropped_hints counter exposes the symptom to operators.
pub const METRICS_CHANNEL_CAP: usize = 10_000;

/// CORR PART 4: bounded, drop-oldest queue used by the metrics
/// collector + training recorder. Cheap to clone — backed by
/// `Arc<Mutex<VecDeque>>` + `Notify` so the sender can evict
/// the oldest entry (tokio::sync::mpsc only supports drop-
/// newest via try_send).
pub struct BoundedDropOldestChannel<T> {
    inner: Arc<BoundedDropOldestInner<T>>,
}

struct BoundedDropOldestInner<T> {
    queue: Mutex<std::collections::VecDeque<T>>,
    notify: tokio::sync::Notify,
    cap: usize,
    dropped: std::sync::atomic::AtomicU64,
    closed: std::sync::atomic::AtomicBool,
}

impl<T> Clone for BoundedDropOldestChannel<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> BoundedDropOldestChannel<T> {
    /// New empty channel with `cap` entries of headroom.
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(BoundedDropOldestInner {
                queue: Mutex::new(std::collections::VecDeque::with_capacity(cap.min(1024))),
                notify: tokio::sync::Notify::new(),
                cap: cap.max(1),
                dropped: std::sync::atomic::AtomicU64::new(0),
                closed: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// Push a value. When the queue is at capacity the
    /// oldest entry is evicted and the dropped counter is
    /// bumped. Never blocks. Wakes one parked receiver.
    pub fn send(&self, value: T) {
        let mut q = self.inner.queue.lock().unwrap_or_else(|e| {
            tracing::warn!("BoundedDropOldestChannel mutex poisoned; recovering inner state");
            e.into_inner()
        });
        while q.len() >= self.inner.cap {
            q.pop_front();
            self.inner
                .dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        q.push_back(value);
        drop(q);
        self.inner.notify.notify_one();
    }

    /// Drain up to `max` entries from the front, returning
    /// them in insertion order. Empty Vec when the queue
    /// is empty.
    pub fn try_drain(&self, max: usize) -> Vec<T> {
        let mut q = self.inner.queue.lock().unwrap_or_else(|e| {
            tracing::warn!("BoundedDropOldestChannel mutex poisoned; recovering inner state");
            e.into_inner()
        });
        let n = max.min(q.len());
        q.drain(..n).collect()
    }

    /// Number of items dropped from the front due to cap
    /// pressure since this channel was created.
    pub fn dropped_count(&self) -> u64 {
        self.inner
            .dropped
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Mark the channel closed and wake any parked receiver
    /// so it can observe the close and exit.
    pub fn close(&self) {
        self.inner
            .closed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Block (async) until the queue has at least one entry or
    /// the channel is closed. Returns `false` when the channel
    /// was closed AND the queue is empty.
    pub async fn wait(&self) -> bool {
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            {
                let q = self.inner.queue.lock().unwrap_or_else(|e| e.into_inner());
                if !q.is_empty() {
                    return true;
                }
                if self.is_closed() {
                    return false;
                }
            }
            notified.await;
        }
    }
}

impl MetricsCollector {
    /// Build a collector around the given `store` + price
    /// table. The drain + retention tasks are spawned via
    /// [`MetricsCollector::spawn_workers`].
    pub fn new(store: MetricsStore, prices: PriceTable) -> (Self, MetricsWorkerHandles) {
        // CORR PART 4: bounded drop-oldest channel replaces the
        // pre-fix unbounded mpsc. A stuck drain task (DB locked,
        // fsync stall) used to let the queue grow without
        // limit; the bounded channel evicts the oldest entry
        // once the cap is hit.
        let channel = BoundedDropOldestChannel::<InvocationMetric>::new(METRICS_CHANNEL_CAP);
        let cap = NonZeroUsize::new(HINT_CACHE_CAP).expect("HINT_CACHE_CAP > 0");
        let collector = Self {
            channel: channel.clone(),
            hints: Arc::new(Mutex::new(LruCache::new(cap))),
            provider_signals: Arc::new(Mutex::new(LruCache::new(cap))),
            self_consistency: Arc::new(Mutex::new(LruCache::new(cap))),
            dropped_hints: Arc::new(AtomicU64::new(0)),
            last_warn_ms: Arc::new(AtomicI64::new(0)),
            prices: Arc::new(prices),
            store: store.clone(),
            budget: Arc::new(Mutex::new(None)),
            absolute_caps: Arc::new(std::sync::OnceLock::new()),
        };
        let handles = MetricsWorkerHandles {
            store,
            channel: Some(channel),
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

    /// PART 4: install the absolute spend caps config + the
    /// alert sink CostAlerts get dispatched through. Idempotent
    /// — only the first call binds; later ones are silently
    /// ignored. Without this wiring the per-request / hourly /
    /// daily checks are no-ops.
    pub fn install_absolute_caps(
        &self,
        cfg: super::spike_detector::CostAlertsConfig,
        sink: Arc<dyn super::alert::AlertDeliver>,
    ) {
        let _ = self.absolute_caps.set(AbsoluteCapsHandle {
            cfg,
            sink,
            state: Arc::new(Mutex::new(AbsoluteCapsState::default())),
        });
    }

    /// PART 4: check an *estimated* per-request cost (USD)
    /// BEFORE dispatch. Returns `true` when the request should
    /// proceed; `false` when it exceeds
    /// `absolute_per_request_cap_usd` and the dispatcher should
    /// fail closed. Fires a CostAlert on rejection.
    pub fn check_per_request_estimate(&self, estimated_usd: f64) -> bool {
        let Some(h) = self.absolute_caps.get() else {
            return true;
        };
        let Some(cap) = h.cfg.absolute_per_request_cap_usd else {
            return true;
        };
        if estimated_usd <= cap {
            return true;
        }
        h.fire(
            "absolute_per_request_cap_exceeded_pre_dispatch",
            cap,
            estimated_usd,
        );
        false
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

    /// CORR PART 4: lifetime count of metrics dropped from the
    /// front of the queue due to cap pressure. Operators read
    /// this via the dashboard to detect a stuck drain task.
    pub fn dropped_count(&self) -> u64 {
        self.channel.dropped_count()
    }

    /// PART 5: lifetime count of hints evicted from the
    /// request-id join LRUs. Bumps on every drop-oldest pop
    /// across all three caches (ai_usage, provider_signals,
    /// self_consistency).
    pub fn dropped_hints(&self) -> u64 {
        self.dropped_hints.load(Ordering::Relaxed)
    }

    /// PART 5: bump dropped counter + emit a rate-limited
    /// warning. At most one warn fires per minute regardless of
    /// how many evictions land in the interim — the counter is
    /// the durable signal; the log line is a heads-up.
    fn note_hint_drop(&self) {
        self.dropped_hints.fetch_add(1, Ordering::Relaxed);
        let now = now_ms();
        let last = self.last_warn_ms.load(Ordering::Relaxed);
        if now - last >= 60_000
            && self
                .last_warn_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            tracing::warn!(
                dropped_total = self.dropped_hints.load(Ordering::Relaxed),
                cap = HINT_CACHE_CAP,
                "metrics.collector: hint cache evicting LRU entries — drain may be falling behind",
            );
        }
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
            // PART 4: actual-cost gate. Fires CostAlert when this
            // single request crossed `absolute_per_request_cap_usd`
            // OR when the rolling hourly / daily windows just
            // tipped past their caps. Independent of any
            // statistical baseline.
            if let Some(h) = self.absolute_caps.get() {
                let cost_usd = (cost as f64) / 1_000_000.0;
                let now_secs = (now_ms() / 1_000).max(0);
                h.observe_actual(cost_usd, now_secs);
            }
        }
        // CORR PART 4: bounded drop-oldest send. Never blocks
        // and never panics; when the queue is at cap the
        // oldest entry is evicted and the `dropped_count`
        // counter is bumped.
        self.channel.send(m);
    }

    fn attach_ai_usage(&self, hint: AiUsageHint) {
        let mut g = match self.hints.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(), // recover from poisoning — losing one row is fine
        };
        // PART 5: drop-oldest-single semantics. When the cache
        // is already at capacity AND the incoming key is new
        // (no replacement) we pop the LRU entry explicitly so
        // we can count + warn on the eviction. The pre-fix code
        // called HashMap::clear() here, dropping every in-flight
        // hint at once on a single overflow.
        let key = hint.request_id;
        let key_present = g.contains(&key);
        let evicted = !key_present && g.len() >= g.cap().get();
        if evicted {
            g.pop_lru();
        }
        g.put(key, hint);
        if evicted {
            self.note_hint_drop();
        }
    }

    fn attach_provider_signals(&self, hint: AiProviderSignalsHint) {
        let mut g = match self.provider_signals.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let key = hint.request_id;
        let key_present = g.contains(&key);
        let evicted = !key_present && g.len() >= g.cap().get();
        if evicted {
            g.pop_lru();
        }
        g.put(key, hint);
        if evicted {
            self.note_hint_drop();
        }
    }

    fn take_provider_signals(&self, request_id: RequestId) -> Option<AiProviderSignalsHint> {
        let mut g = match self.provider_signals.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.pop(&request_id)
    }

    fn attach_self_consistency(&self, hint: AiSelfConsistencyHint) {
        let mut g = match self.self_consistency.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let key = hint.request_id;
        let key_present = g.contains(&key);
        let evicted = !key_present && g.len() >= g.cap().get();
        if evicted {
            g.pop_lru();
        }
        g.put(key, hint);
        if evicted {
            self.note_hint_drop();
        }
    }

    fn take_self_consistency(&self, request_id: RequestId) -> Option<AiSelfConsistencyHint> {
        let mut g = match self.self_consistency.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.pop(&request_id)
    }
}

fn take_hint(
    hints: &Arc<Mutex<LruCache<RequestId, AiUsageHint>>>,
    req_id: &RequestId,
) -> Option<AiUsageHint> {
    let mut g = match hints.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    g.pop(req_id)
}

/// PART 4: rolling-window state used by the absolute spend caps.
/// One deque per cap window. Pruned on every observation, so the
/// memory footprint stays bounded at ~one entry per cost-bearing
/// metric in the last 24h.
#[derive(Default)]
struct AbsoluteCapsState {
    hourly_events: std::collections::VecDeque<(i64, f64)>,
    daily_events: std::collections::VecDeque<(i64, f64)>,
}

/// PART 4: handle installed on the collector. Bundles the config,
/// the alert sink to deliver CostAlerts through, and the rolling
/// windows. Cheap to clone (Arc-wrapped state).
struct AbsoluteCapsHandle {
    cfg: super::spike_detector::CostAlertsConfig,
    sink: Arc<dyn super::alert::AlertDeliver>,
    state: Arc<Mutex<AbsoluteCapsState>>,
}

impl AbsoluteCapsHandle {
    fn observe_actual(&self, cost_usd: f64, now_secs: i64) {
        // Per-request actual cost check.
        if let Some(cap) = self.cfg.absolute_per_request_cap_usd
            && cost_usd > cap
        {
            self.fire("absolute_per_request_cap_exceeded_actual", cap, cost_usd);
        }
        // Roll into the hourly + daily windows and check sums.
        let mut g = match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.hourly_events.push_back((now_secs, cost_usd));
        g.daily_events.push_back((now_secs, cost_usd));
        let hour_cutoff = now_secs - 3_600;
        let day_cutoff = now_secs - 86_400;
        while let Some(&(t, _)) = g.hourly_events.front() {
            if t < hour_cutoff {
                g.hourly_events.pop_front();
            } else {
                break;
            }
        }
        while let Some(&(t, _)) = g.daily_events.front() {
            if t < day_cutoff {
                g.daily_events.pop_front();
            } else {
                break;
            }
        }
        let hourly_sum: f64 = g.hourly_events.iter().map(|(_, c)| *c).sum();
        let daily_sum: f64 = g.daily_events.iter().map(|(_, c)| *c).sum();
        drop(g);
        if let Some(cap) = self.cfg.absolute_hourly_cap_usd
            && hourly_sum > cap
        {
            self.fire("absolute_hourly_cap_exceeded", cap, hourly_sum);
        }
        if let Some(cap) = self.cfg.absolute_daily_cap_usd
            && daily_sum > cap
        {
            self.fire("absolute_daily_cap_exceeded", cap, daily_sum);
        }
    }

    fn fire(&self, cause: &'static str, cap_usd: f64, actual_usd: f64) {
        let now_ms = now_ms();
        let event = super::alert::AlertEvent::Fired(super::alert::ActiveAlert {
            agent: "deployment".to_string(),
            kind: super::alert::AlertKind::CostAlert,
            severity: super::alert::AlertSeverity::Critical,
            triggered_at_ms: now_ms,
            threshold: cap_usd,
            actual: actual_usd,
            message: cause.to_string(),
            method: None,
        });
        self.sink.deliver(&event);
        tracing::warn!(
            cause,
            cap_usd,
            actual_usd,
            "metrics.cost_alerts: absolute spend cap exceeded"
        );
    }
}

/// Owned worker handles returned by [`MetricsCollector::new`].
/// Call [`spawn`](Self::spawn) once on startup to start the
/// drain + retention loops. Drops cleanly on shutdown — the
/// drain loop exits when the collector's sender is dropped.
pub struct MetricsWorkerHandles {
    store: MetricsStore,
    /// CORR PART 4: the receiver side of the bounded drop-
    /// oldest channel. Holds an `Arc` clone of the same
    /// channel the collector pushes into.
    channel: Option<BoundedDropOldestChannel<InvocationMetric>>,
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
        let channel = self
            .channel
            .expect("MetricsWorkerHandles::spawn called twice");
        let drain_store = self.store.clone();
        let retention_store = self.store.clone();
        let drain = tokio::spawn(async move {
            run_drain_loop(channel, drain_store).await;
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

async fn run_drain_loop(channel: BoundedDropOldestChannel<InvocationMetric>, store: MetricsStore) {
    let mut batch: Vec<InvocationMetric> = Vec::with_capacity(BATCH_SIZE);
    let mut tick = tokio::time::interval(Duration::from_millis(BATCH_INTERVAL_MS));
    // Skip the first immediate tick — interval fires at t=0.
    tick.tick().await;
    loop {
        // CORR PART 4: drain the bounded channel in batches.
        // Drain on either a new push (notify) or a flush
        // tick, whichever comes first.
        tokio::select! {
            biased;
            present = channel.wait() => {
                if !present {
                    // Channel closed AND queue empty.
                    flush_batch(&store, &mut batch);
                    return;
                }
                let drained = channel.try_drain(BATCH_SIZE);
                for m in drained {
                    batch.push(m);
                    if batch.len() >= BATCH_SIZE {
                        flush_batch(&store, &mut batch);
                    }
                }
            }
            _ = tick.tick() => {
                let drained = channel.try_drain(BATCH_SIZE);
                for m in drained {
                    batch.push(m);
                }
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
    channel: &BoundedDropOldestChannel<InvocationMetric>,
    store: &MetricsStore,
) -> Result<usize, MetricsStoreError> {
    let batch = channel.try_drain(usize::MAX);
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
            tenant_id: "default".into(),
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

    /// PART 5: drop-oldest LRU semantics. The pre-fix path
    /// cleared the entire cache when overflowed; the LRU now
    /// evicts only the oldest entry per insertion and bumps
    /// the `dropped_hints` counter on every eviction.
    #[test]
    fn part5_hint_cache_drops_only_oldest_on_overflow() {
        let store = MetricsStore::in_memory().unwrap();
        let prices = PriceTable::with_defaults();
        let (col, _h) = MetricsCollector::new(store, prices);
        fn unique_rid(i: usize) -> RequestId {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&(i as u64).to_le_bytes());
            RequestId(b)
        }
        let extra = 10;
        for i in 0..(HINT_CACHE_CAP + extra) {
            col.attach_ai_usage(AiUsageHint {
                request_id: unique_rid(i),
                prompt_tokens: 1,
                completion_tokens: 1,
                model: "mock".into(),
                routing_tier: None,
            });
        }
        // The LRU stays full at the cap (not "≤ extra") because
        // each insertion past the cap evicts only the LRU
        // entry, never the entire cache.
        let g = col.hints.lock().unwrap();
        assert_eq!(
            g.len(),
            HINT_CACHE_CAP,
            "expected cache to stay at cap, got {}",
            g.len()
        );
        drop(g);
        assert_eq!(
            col.dropped_hints(),
            extra as u64,
            "expected dropped_hints to count each LRU eviction"
        );
        // And the most recent entries are still resident — the
        // pre-fix clear-all would have dropped them too.
        let g = col.hints.lock().unwrap();
        assert!(
            g.contains(&unique_rid(HINT_CACHE_CAP + extra - 1)),
            "newest entry must still be in the cache"
        );
    }

    // ── CORR PART 4: bounded drop-oldest channel ─────────

    #[test]
    fn corr_p4_bounded_channel_drops_oldest_on_overflow() {
        // Push cap+5 entries; expect the channel to keep the
        // most recent `cap` and report 5 drops.
        let cap = 4usize;
        let ch = BoundedDropOldestChannel::<u32>::new(cap);
        for i in 0..(cap + 5) {
            ch.send(i as u32);
        }
        let drained = ch.try_drain(100);
        assert_eq!(drained, vec![5, 6, 7, 8], "must keep the newest cap items");
        assert_eq!(ch.dropped_count(), 5, "must report all evictions");
    }

    #[tokio::test]
    async fn corr_p4_bounded_channel_wait_returns_false_on_close_empty() {
        let ch = BoundedDropOldestChannel::<u32>::new(4);
        ch.close();
        let present = ch.wait().await;
        assert!(!present);
    }

    // ── CORR-D3: explicit verification per the prompt ────

    #[test]
    fn corr_d3_send_at_cap_evicts_oldest() {
        // Channel at capacity: the next `send` drops the
        // oldest entry (not the new one).
        let cap = 3usize;
        let ch = BoundedDropOldestChannel::<u32>::new(cap);
        ch.send(10);
        ch.send(11);
        ch.send(12);
        assert_eq!(ch.dropped_count(), 0, "no drops while under cap");
        ch.send(13);
        // 10 was dropped; the remaining set is {11, 12, 13}.
        assert_eq!(ch.dropped_count(), 1);
        let drained = ch.try_drain(100);
        assert_eq!(drained, vec![11, 12, 13]);
    }

    #[test]
    fn corr_d3_dropped_count_increments_on_every_drop() {
        // 10 consecutive over-cap sends → 10 drops, dropped
        // counter = 10.
        let cap = 2usize;
        let ch = BoundedDropOldestChannel::<u32>::new(cap);
        ch.send(0);
        ch.send(1);
        let mut expected = 0u64;
        for v in 2..12u32 {
            ch.send(v);
            expected += 1;
            assert_eq!(
                ch.dropped_count(),
                expected,
                "counter must bump exactly once per evicted entry"
            );
        }
        // Queue still holds the most-recent two entries.
        let drained = ch.try_drain(100);
        assert_eq!(drained, vec![10, 11]);
    }

    #[test]
    fn corr_d3_receiver_sees_insertion_order_after_overflow() {
        // After overflow, try_drain returns items in
        // insertion order — the queue is FIFO and only the
        // OLDEST entries were evicted.
        let cap = 4usize;
        let ch = BoundedDropOldestChannel::<&'static str>::new(cap);
        for v in ["a", "b", "c", "d", "e", "f", "g"] {
            ch.send(v);
        }
        // Dropped a, b, c → remaining {d, e, f, g}.
        let drained = ch.try_drain(100);
        assert_eq!(drained, vec!["d", "e", "f", "g"]);
        assert_eq!(ch.dropped_count(), 3);
    }

    #[test]
    fn corr_d3_no_loss_except_for_explicitly_dropped_entries() {
        // Total received_at_receiver + total_dropped must
        // equal total_sent for every prefix of the send
        // stream. Demonstrates that the channel does not
        // silently drop anything other than the over-cap
        // evictions accounted for in dropped_count.
        let cap = 5usize;
        let ch = BoundedDropOldestChannel::<u32>::new(cap);
        let total_sent: u64 = 50;
        for i in 0..total_sent as u32 {
            ch.send(i);
        }
        let received = ch.try_drain(usize::MAX);
        let dropped = ch.dropped_count();
        assert_eq!(
            received.len() as u64 + dropped,
            total_sent,
            "received + dropped must equal sent (no silent loss)"
        );
        // Received items are the last `cap` sent.
        let expected: Vec<u32> = ((total_sent as u32 - cap as u32)..total_sent as u32).collect();
        assert_eq!(received, expected);
    }
}
