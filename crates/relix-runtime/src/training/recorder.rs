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

use super::pii::PiiAnonymizer;
use super::store::TrainingStore;
use super::types::{InteractionRecord, ToolCallRecord};

/// Trait the AI handler holds. Stripped down so non-recording
/// builds can use [`NullInteractionSink`] without dragging in
/// SQLite.
pub trait InteractionSink: Send + Sync {
    fn record_interaction(&self, rec: InteractionRecord);
}

/// Production sink — non-blocking mpsc producer in front of the
/// shared drain task. Cheap to clone.
///
/// The recorder holds an `Arc<PiiAnonymizer>` so the record-time
/// anonymization pass is a single Arc deref + an optional regex
/// scan on the hot path. When the anonymizer is disabled (the
/// default), `record_interaction` is byte-identical to the
/// pre-PII recorder shape.
///
/// Per-agent training opt-in lives on
/// [`AgentTrainingPolicies`]: an `Arc<BTreeMap<agent,
/// AgentTrainingPolicy>>` the recorder consults before
/// persisting. Agents whose policy says `enabled = false` are
/// dropped at the sink boundary (no row in `training.sqlite`,
/// no mpsc send, no drain-task work). Agents with a
/// `pii_strategy` override pre-bind a per-agent anonymizer that
/// the recorder uses instead of the global one.
#[derive(Clone)]
pub struct InteractionRecorder {
    /// CORR PART 4: bounded drop-oldest channel replaces the
    /// pre-fix `mpsc::UnboundedSender`. A stuck drain task
    /// (DB locked, fsync stall) used to let interactions
    /// queue without limit; the bounded channel evicts the
    /// oldest entry at [`TRAINING_CHANNEL_CAP`] and exposes
    /// the dropped count for operator dashboards.
    channel: crate::metrics::collector::BoundedDropOldestChannel<InteractionRecord>,
    store: TrainingStore,
    anonymizer: Arc<PiiAnonymizer>,
    agent_policies: Arc<AgentTrainingPolicies>,
}

/// CORR PART 4: hard cap on the in-flight training-recorder
/// queue. Same posture as [`crate::metrics::collector::METRICS_CHANNEL_CAP`].
pub const TRAINING_CHANNEL_CAP: usize = 10_000;

pub const BATCH_INTERVAL_MS: u64 = 100;
pub const BATCH_SIZE: usize = 100;

/// Per-agent training opt-in + PII strategy overrides.
///
/// Keyed by `agent` (the friendly name on the caller's
/// `IdentityBundle.name`). An empty map / `enabled=true`
/// policy means the agent inherits the global behaviour.
#[derive(Clone, Debug, Default)]
pub struct AgentTrainingPolicies {
    /// `enabled` defaults to `true` so an agent with no
    /// explicit entry is recorded as before. An explicit
    /// `enabled=false` entry skips the agent at the sink
    /// boundary.
    pub enabled: std::collections::BTreeMap<String, bool>,
    /// Pre-resolved per-agent anonymizers. Built from the
    /// global PII config + an agent-specific `pii_strategy`
    /// override; cached as `Arc<PiiAnonymizer>` so the hot
    /// path is a single map lookup.
    pub anonymizers: std::collections::BTreeMap<String, Arc<PiiAnonymizer>>,
}

impl AgentTrainingPolicies {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Returns `false` only when the agent has an explicit
    /// `enabled = false` entry. Unknown agents are enabled
    /// by default.
    pub fn enabled_for(&self, agent: &str) -> bool {
        self.enabled.get(agent).copied().unwrap_or(true)
    }

    pub fn anonymizer_for(&self, agent: &str) -> Option<Arc<PiiAnonymizer>> {
        self.anonymizers.get(agent).cloned()
    }
}

impl InteractionRecorder {
    /// Construct a recorder with a disabled anonymizer + empty
    /// per-agent policies. Mirrors the pre-7.15-PII shape so
    /// existing tests + callers continue to compile.
    pub fn new(store: TrainingStore) -> (Self, RecorderWorkerHandles) {
        Self::new_with(
            store,
            Arc::new(PiiAnonymizer::disabled()),
            Arc::new(AgentTrainingPolicies::empty()),
        )
    }

    /// Full constructor — operators wire this from the
    /// controller-runtime training bundle.
    pub fn new_with(
        store: TrainingStore,
        anonymizer: Arc<PiiAnonymizer>,
        agent_policies: Arc<AgentTrainingPolicies>,
    ) -> (Self, RecorderWorkerHandles) {
        let channel = crate::metrics::collector::BoundedDropOldestChannel::<InteractionRecord>::new(
            TRAINING_CHANNEL_CAP,
        );
        (
            Self {
                channel: channel.clone(),
                store: store.clone(),
                anonymizer,
                agent_policies,
            },
            RecorderWorkerHandles {
                store,
                channel: Some(channel),
            },
        )
    }

    pub fn store(&self) -> TrainingStore {
        self.store.clone()
    }

    /// CORR PART 4: lifetime count of records dropped from the
    /// front of the queue due to cap pressure.
    pub fn dropped_count(&self) -> u64 {
        self.channel.dropped_count()
    }
}

impl InteractionSink for InteractionRecorder {
    fn record_interaction(&self, mut rec: InteractionRecord) {
        // Per-agent opt-in: drop the record entirely if the
        // operator has explicitly opted this agent OUT of
        // training capture. We do this BEFORE anonymization so
        // disabled agents pay zero CPU cost on the hot path.
        if !self.agent_policies.enabled_for(&rec.agent) {
            return;
        }
        if rec.recorded_at == 0 {
            rec.recorded_at = now_ms();
        }
        // Choose the effective anonymizer: a per-agent
        // override wins, otherwise the global one applies.
        let active = self
            .agent_policies
            .anonymizer_for(&rec.agent)
            .unwrap_or_else(|| self.anonymizer.clone());
        if active.enabled() {
            apply_anonymizer(&mut rec, &active);
        }
        // CORR PART 4: bounded drop-oldest send. Never blocks
        // and never panics; when the queue is at cap the
        // oldest entry is evicted and the channel's lifetime
        // dropped_count is bumped.
        self.channel.send(rec);
    }
}

/// Run the anonymizer over every field that may contain raw
/// user content (system_prompt + user_message + response + each
/// tool call's input + output) and flip `rec.anonymized` to
/// true. Idempotent on already-anonymized records (running
/// twice has no observable effect because the anonymizer's
/// placeholders / pseudonyms aren't themselves valid PII).
pub fn apply_anonymizer(rec: &mut InteractionRecord, anon: &PiiAnonymizer) {
    if !anon.enabled() {
        return;
    }
    rec.system_prompt = anon.anonymize(&rec.system_prompt);
    rec.user_message = anon.anonymize(&rec.user_message);
    rec.response = anon.anonymize(&rec.response);
    for c in rec.tool_calls.iter_mut() {
        c.input = anon.anonymize(&c.input);
        c.output = anon.anonymize(&c.output);
    }
    rec.anonymized = true;
}

/// Helper for the export engine + tests: anonymize the
/// fields of a record in-place without setting `anonymized =
/// true` on the original (the caller decides whether to mark
/// or not). Returns a copy.
pub fn anonymize_record(rec: &InteractionRecord, anon: &PiiAnonymizer) -> InteractionRecord {
    let mut copy = rec.clone();
    if anon.enabled() {
        copy.system_prompt = anon.anonymize(&copy.system_prompt);
        copy.user_message = anon.anonymize(&copy.user_message);
        copy.response = anon.anonymize(&copy.response);
        let tcs: Vec<ToolCallRecord> = copy
            .tool_calls
            .into_iter()
            .map(|mut c| {
                c.input = anon.anonymize(&c.input);
                c.output = anon.anonymize(&c.output);
                c
            })
            .collect();
        copy.tool_calls = tcs;
        copy.anonymized = true;
    }
    copy
}

/// Owned worker handles returned by
/// [`InteractionRecorder::new`]. Call
/// [`Self::spawn`](Self::spawn) once to start the drain +
/// retention loops.
pub struct RecorderWorkerHandles {
    store: TrainingStore,
    /// CORR PART 4: bounded channel handle the drain loop
    /// consumes from. `Option` so the spawn call can move it
    /// out exactly once.
    channel: Option<crate::metrics::collector::BoundedDropOldestChannel<InteractionRecord>>,
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
        let channel = self
            .channel
            .expect("RecorderWorkerHandles::spawn called twice");
        let drain_store = self.store.clone();
        let retention_store = self.store.clone();
        let drain = tokio::spawn(async move {
            run_drain_loop(channel, drain_store).await;
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

async fn run_drain_loop(
    channel: crate::metrics::collector::BoundedDropOldestChannel<InteractionRecord>,
    store: TrainingStore,
) {
    let mut batch: Vec<InteractionRecord> = Vec::with_capacity(BATCH_SIZE);
    let mut tick = tokio::time::interval(Duration::from_millis(BATCH_INTERVAL_MS));
    tick.tick().await;
    loop {
        tokio::select! {
            biased;
            present = channel.wait() => {
                if !present {
                    flush_batch(&store, &mut batch);
                    return;
                }
                let drained = channel.try_drain(BATCH_SIZE);
                for rec in drained {
                    batch.push(rec);
                    if batch.len() >= BATCH_SIZE {
                        flush_batch(&store, &mut batch);
                    }
                }
            }
            _ = tick.tick() => {
                let drained = channel.try_drain(BATCH_SIZE);
                for rec in drained {
                    batch.push(rec);
                }
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
    use super::super::pii::{PiiConfig, PiiStrategy};
    use super::super::types::{InteractionId, InteractionRecord, ToolCallRecord};
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
            anonymized: false,
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

    // ── RELIX-7.15 PII integration ─────────────────────────

    fn pii_record_with_email_in_user_message(id: &str) -> InteractionRecord {
        let mut r = record(id, "alice", 100);
        r.system_prompt = "you are alice".into();
        r.user_message = "please email alice@example.com".into();
        r.response = "ok, will email alice@example.com".into();
        r.tool_calls = vec![ToolCallRecord {
            tool: "email.send".into(),
            input: "to: alice@example.com".into(),
            output: "sent: alice@example.com".into(),
            success: true,
            latency_ms: 5,
            error_kind: None,
        }];
        r
    }

    fn redact_anon() -> Arc<PiiAnonymizer> {
        Arc::new(PiiAnonymizer::from_config(&PiiConfig {
            enabled: true,
            strategy: PiiStrategy::Redact,
            overrides: Default::default(),
        }))
    }

    #[tokio::test]
    async fn anonymized_recorder_strips_pii_before_persisting() {
        let store = TrainingStore::in_memory().unwrap();
        let policies = Arc::new(AgentTrainingPolicies::empty());
        let (rec, handles) = InteractionRecorder::new_with(store.clone(), redact_anon(), policies);
        let _h = handles.spawn(RetentionConfig::default());
        rec.record_interaction(pii_record_with_email_in_user_message("redacted-1"));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let got = store.get("redacted-1").unwrap().unwrap();
        // Email must NOT survive on any of the redacted fields.
        for field in [
            &got.system_prompt,
            &got.user_message,
            &got.response,
            &got.tool_calls[0].input,
            &got.tool_calls[0].output,
        ] {
            assert!(
                !field.contains("alice@example.com"),
                "raw PII survived: {field:?}",
            );
        }
        // user_message + response carried the email — both
        // must now contain the placeholder.
        assert!(got.user_message.contains("[EMAIL]"));
        assert!(got.response.contains("[EMAIL]"));
        // Recorder must flip the `anonymized` flag so the
        // export engine knows not to re-run.
        assert!(got.anonymized, "recorder must flip anonymized = true");
        drop(rec);
    }

    #[tokio::test]
    async fn anonymization_disabled_keeps_raw_text_and_unset_flag() {
        let store = TrainingStore::in_memory().unwrap();
        let (rec, handles) = InteractionRecorder::new(store.clone());
        let _h = handles.spawn(RetentionConfig::default());
        rec.record_interaction(pii_record_with_email_in_user_message("plain-1"));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let got = store.get("plain-1").unwrap().unwrap();
        assert!(got.user_message.contains("alice@example.com"));
        assert!(!got.anonymized, "no anonymizer → no flag flip");
        drop(rec);
    }

    #[tokio::test]
    async fn agent_with_training_disabled_drops_records_at_sink_boundary() {
        let store = TrainingStore::in_memory().unwrap();
        let mut policies = AgentTrainingPolicies::empty();
        policies.enabled.insert("public-agent".into(), false);
        let (rec, handles) = InteractionRecorder::new_with(
            store.clone(),
            Arc::new(PiiAnonymizer::disabled()),
            Arc::new(policies),
        );
        let _h = handles.spawn(RetentionConfig::default());
        rec.record_interaction(record("kept", "work-agent", 100));
        rec.record_interaction(record("dropped", "public-agent", 200));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(store.get("kept").unwrap().is_some());
        assert!(
            store.get("dropped").unwrap().is_none(),
            "disabled agent must not produce a row"
        );
        drop(rec);
    }

    #[tokio::test]
    async fn per_agent_pii_strategy_overrides_global_strategy() {
        let store = TrainingStore::in_memory().unwrap();
        // Global anonymizer is the disabled instance; only
        // the `work-agent` policy carries a real one.
        let mut policies = AgentTrainingPolicies::empty();
        policies
            .anonymizers
            .insert("work-agent".into(), redact_anon());
        let (rec, handles) = InteractionRecorder::new_with(
            store.clone(),
            Arc::new(PiiAnonymizer::disabled()),
            Arc::new(policies),
        );
        let _h = handles.spawn(RetentionConfig::default());
        let mut scoped_rec = pii_record_with_email_in_user_message("scoped");
        scoped_rec.agent = "work-agent".into();
        rec.record_interaction(scoped_rec);
        let mut unscoped_rec = pii_record_with_email_in_user_message("unscoped");
        unscoped_rec.agent = "other-agent".into();
        rec.record_interaction(unscoped_rec);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let scoped = store.get("scoped").unwrap().unwrap();
        let unscoped = store.get("unscoped").unwrap().unwrap();
        // work-agent → per-agent anonymizer redacts.
        assert!(scoped.user_message.contains("[EMAIL]"));
        assert!(scoped.anonymized);
        // other-agent → global anonymizer is disabled → raw.
        assert!(unscoped.user_message.contains("alice@example.com"));
        assert!(!unscoped.anonymized);
        drop(rec);
    }
}
