//! W2-MEMORY-CURATOR — periodic LLM-driven curation of
//! per-subject persistent memory.
//!
//! Patterned on Hermes's curator subsystem
//! (`agent/curator.py`, 1781 lines) but scoped to what Relix
//! needs now: review the agent + user memory for each
//! `subject_id`, consolidate redundant entries, drop stale
//! ones, keep memory lean and useful.
//!
//! ## Components
//!
//! - [`AiDispatcher`] trait — async hook that calls `ai.chat`.
//!   Production wraps a [`MeshClient`] pointing at the AI peer.
//!   Tests stub it.
//! - [`AiMeshDispatcher`] — the live impl.
//! - [`CuratorState`] — shared in-memory status (last run,
//!   summary, running flag). Queried by `/v1/memory/curator/status`.
//! - [`curate_subject`] — pure-logic curation of one subject.
//!   Used by the manual `memory.agent_curate` capability and
//!   by the background scheduler.
//! - [`spawn_curator_scheduler`] — the periodic tick task.
//!
//! ## Failure mode
//!
//! Every error path inside a curator pass is **silent skip**
//! — the operator's existing memory must NEVER be wiped or
//! corrupted by a curator failure. If the AI peer returns an
//! empty response, an over-cap response, or anything we can't
//! parse, we leave the target untouched and continue with the
//! next agent.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::dispatch::{build_request, decode_response};
use crate::manifest::MeshClient;
use crate::transport::envelope::ResponseResult;
use relix_core::bundle::Bundle;

use super::{
    AGENT_MEMORY_CAP_CHARS, ENTRY_DELIMITER, MemoryError, MemoryStore, USER_MEMORY_CAP_CHARS,
};

// ───────────────────────── Config ───────────────────────────────

/// Per-node curator configuration parsed from `[memory.curator]`.
#[derive(Clone, Debug, Deserialize)]
pub struct CuratorConfig {
    /// Master switch. When `false`, the scheduler is not
    /// spawned and the `memory.agent_curate` capability still
    /// works (it's manual / on-demand).
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Seconds between scheduler ticks. Default 1 hour.
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
    /// Agents with combined (agent + user) char count at or
    /// below this threshold are skipped — nothing to curate.
    #[serde(default = "default_min_chars")]
    pub min_chars_to_curate: usize,
    /// Optional outbound peer pointing at the AI node. When
    /// absent, the curator scheduler doesn't start AND the
    /// manual capability returns `BackendNotConnected`.
    #[serde(default, rename = "ai_peer")]
    pub ai_peer: Option<AiPeerConfig>,
}

impl Default for CuratorConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            interval_secs: default_interval_secs(),
            min_chars_to_curate: default_min_chars(),
            ai_peer: None,
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_interval_secs() -> u64 {
    3600 // 1 hour
}

fn default_min_chars() -> usize {
    100
}

/// `[memory.curator.ai_peer]` — names the AI peer the curator
/// should dial.
#[derive(Clone, Debug, Deserialize)]
pub struct AiPeerConfig {
    /// libp2p multiaddr (e.g. `/ip4/127.0.0.1/tcp/19712`).
    pub addr: String,
    /// Alias the outbound MeshClient uses to dial. Defaults
    /// to `"ai"`.
    #[serde(default = "default_ai_alias")]
    pub alias: String,
    /// Per-call deadline in seconds. `ai.chat` is slow — give
    /// it room. Default 30.
    #[serde(default = "default_ai_deadline_secs")]
    pub deadline_secs: i64,
}

fn default_ai_alias() -> String {
    "ai".to_string()
}

fn default_ai_deadline_secs() -> i64 {
    30
}

// ───────────────────────── AiDispatcher ────────────────────────

/// Async hook the curator reaches through to call `ai.chat`.
/// Production wraps a `MeshClient`; tests stub it directly.
#[async_trait]
pub trait AiDispatcher: Send + Sync {
    /// Return the model's reply text on success, or `None`
    /// on any failure (network, decode, responder err).
    /// Curator silently skips memory updates on `None`.
    async fn chat(&self, session_id: &str, prompt: &str, history: &str) -> Option<String>;
}

/// Live `AiDispatcher` implementation — wraps a `MeshClient`
/// pointing at the AI peer. Built by the memory controller at
/// startup via `discover_and_pin`, same pattern the AI node
/// uses to dial memory in W2-MEMORY-2.
#[derive(Clone)]
pub struct AiMeshDispatcher {
    mesh: MeshClient,
    alias: String,
    identity: Bundle,
    deadline_secs: i64,
}

impl AiMeshDispatcher {
    pub fn new(mesh: MeshClient, alias: String, identity: Bundle, deadline_secs: i64) -> Self {
        Self {
            mesh,
            alias,
            identity,
            deadline_secs,
        }
    }
}

#[async_trait]
impl AiDispatcher for AiMeshDispatcher {
    async fn chat(&self, session_id: &str, prompt: &str, history: &str) -> Option<String> {
        // ai.chat wire format: `session_id|prompt|history`.
        // History may contain `|` (splitn(3) on the responder
        // side handles it); prompt may also, so we just
        // concatenate raw — receiver's parser is tolerant.
        let mut arg = String::with_capacity(session_id.len() + prompt.len() + history.len() + 4);
        arg.push_str(session_id);
        arg.push('|');
        arg.push_str(prompt);
        arg.push('|');
        arg.push_str(history);
        let envelope = build_request(
            "ai.chat",
            arg.into_bytes(),
            self.identity.clone(),
            self.deadline_secs,
        );
        let resp_bytes = match self.mesh.call(&self.alias, envelope).await {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(
                    alias = %self.alias,
                    error = %e,
                    "curator ai.chat fetch failed (silent skip)"
                );
                return None;
            }
        };
        let resp = decode_response(&resp_bytes).ok()?;
        match resp.res {
            ResponseResult::Ok(body) => String::from_utf8(body.to_vec()).ok(),
            ResponseResult::Err(env) => {
                tracing::debug!(
                    alias = %self.alias,
                    cause = %env.cause,
                    "curator ai.chat err response (silent skip)"
                );
                None
            }
            ResponseResult::StreamHandle(_) => None,
        }
    }
}

// ───────────────────────── State ───────────────────────────────

/// In-memory status shared between the scheduler, the
/// `memory.agent_curate` handler, and the bridge's
/// `/v1/memory/curator/status` proxy.
#[derive(Debug, Default, Clone)]
pub struct CuratorState {
    /// Unix seconds of the last scheduler run's start. None
    /// until the first tick has fired (or after a fresh boot
    /// with no manual call yet).
    pub last_run_at: Option<i64>,
    /// Summary of the last scheduler run.
    pub last_run_summary: Option<CuratorRunSummary>,
    /// Unix seconds of the next scheduled tick.
    pub next_run_at: Option<i64>,
    /// True while a scheduler tick is in progress. Used as a
    /// concurrency guard — a second tick that lands while the
    /// previous one is still going will skip cleanly.
    pub running: bool,
}

/// Per-run telemetry the scheduler writes back into [`CuratorState`].
#[derive(Debug, Default, Clone)]
pub struct CuratorRunSummary {
    pub agents_reviewed: usize,
    pub agents_curated: usize,
    pub total_chars_saved: usize,
}

/// Per-subject curation summary returned by [`curate_subject`].
/// Carries both targets' before/after counts so the manual
/// capability + bridge endpoint can render an informative
/// reply.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CuratorSubjectResult {
    pub subject_id: String,
    pub agent_entries_before: usize,
    pub agent_entries_after: usize,
    pub agent_chars_before: usize,
    pub agent_chars_after: usize,
    pub user_entries_before: usize,
    pub user_entries_after: usize,
    pub user_chars_before: usize,
    pub user_chars_after: usize,
}

impl CuratorSubjectResult {
    pub fn chars_saved(&self) -> usize {
        self.agent_chars_before
            .saturating_sub(self.agent_chars_after)
            + self.user_chars_before.saturating_sub(self.user_chars_after)
    }

    /// Render as a pipe-delimited key=value text body — the
    /// shape `memory.agent_curate` returns on the wire.
    pub fn to_wire(&self) -> String {
        format!(
            "subject_id={}|agent_entries_before={}|agent_entries_after={}|agent_chars_before={}|agent_chars_after={}|user_entries_before={}|user_entries_after={}|user_chars_before={}|user_chars_after={}|chars_saved={}\n",
            self.subject_id,
            self.agent_entries_before,
            self.agent_entries_after,
            self.agent_chars_before,
            self.agent_chars_after,
            self.user_entries_before,
            self.user_entries_after,
            self.user_chars_before,
            self.user_chars_after,
            self.chars_saved(),
        )
    }
}

// ───────────────────────── Curation logic ─────────────────────

/// Errors specific to a curation pass. Curator callers map
/// most of these to a silent-skip / log-only path so a single
/// agent's bad state never wedges the scheduler.
#[derive(Debug, thiserror::Error)]
pub enum CuratorError {
    /// Store-level error (lock, db, io). Propagated.
    #[error("store: {0}")]
    Store(#[from] MemoryError),
    /// AI peer returned no response — silent skip per spec.
    #[error("ai peer unavailable")]
    AiUnavailable,
    /// Curator rejected the AI response: empty, over-cap, or
    /// invalid format (delimiter rules). The existing memory
    /// is left untouched.
    #[error("ai response rejected: {0}")]
    AiResponseRejected(String),
}

/// Number of non-empty entries in a target's content. An
/// empty string yields 0 entries; a single non-empty target
/// with no delimiters yields 1. The trailing-empty case
/// shouldn't happen (caller never writes one), but we filter
/// to be safe.
pub fn count_entries(content: &str) -> usize {
    if content.is_empty() {
        return 0;
    }
    content
        .split(ENTRY_DELIMITER)
        .filter(|s| !s.is_empty())
        .count()
}

/// Build the curation prompt for one target. The format is
/// exact and tested — operators reading agent logs can pick
/// the prompt out verbatim.
pub fn build_curation_prompt(content: &str, cap: usize) -> String {
    format!(
        "Curate the following agent memory. Rules:\n\
         1. Remove duplicate or near-duplicate entries\n\
         2. Consolidate related entries into one clear entry\n\
         3. Remove entries that are outdated or no longer useful\n\
         4. Keep entries that are specific and actionable\n\
         5. Preserve § as the delimiter between entries\n\
         6. Stay within {cap} characters total\n\
         7. Return ONLY the curated entries separated by §, nothing else\n\
         \n\
         Current entries:\n\
         {content}"
    )
}

/// Curation system context — injected as the chat history so
/// the AI sees it as session context per Hermes's
/// MemoryGuidance pattern.
pub const CURATION_SYSTEM_CONTEXT: &str = "You are a memory curator for an AI agent. Your job is to clean up the agent's persistent memory by removing duplicates, consolidating related entries, and removing stale information. Always preserve the § character as the entry delimiter. Never exceed the character cap. Return only the curated content with no explanation or preamble.";

/// Curate one target. Returns `Ok(new_content)` on success or
/// a `CuratorError` describing why the existing content was
/// left untouched.
pub async fn curate_one_target(
    ai: &dyn AiDispatcher,
    subject_id: &str,
    target: &str,
    current: &str,
    cap: usize,
) -> Result<String, CuratorError> {
    if current.is_empty() {
        return Ok(String::new());
    }
    let prompt = build_curation_prompt(current, cap);
    let session_id = format!("curate-{subject_id}-{target}");
    let reply = ai
        .chat(&session_id, &prompt, CURATION_SYSTEM_CONTEXT)
        .await
        .ok_or(CuratorError::AiUnavailable)?;
    let trimmed = reply.trim().to_string();
    if trimmed.is_empty() {
        return Err(CuratorError::AiResponseRejected(
            "empty reply — refusing to wipe existing memory".into(),
        ));
    }
    let char_count = trimmed.chars().count();
    if char_count > cap {
        return Err(CuratorError::AiResponseRejected(format!(
            "curated content {char_count} chars exceeds cap {cap}; existing memory kept"
        )));
    }
    Ok(trimmed)
}

/// Curate one subject end-to-end: read both targets, ask the
/// AI to curate each, write back the survivors. Either target
/// being empty short-circuits to a no-op for that target. AI
/// failures on one target don't affect the other.
pub async fn curate_subject(
    store: &MemoryStore,
    ai: &dyn AiDispatcher,
    subject_id: &str,
) -> Result<CuratorSubjectResult, CuratorError> {
    let (agent_before, user_before) = store.agent_read(subject_id)?;
    let agent_entries_before = count_entries(&agent_before);
    let user_entries_before = count_entries(&user_before);
    let agent_chars_before = agent_before.chars().count();
    let user_chars_before = user_before.chars().count();

    // Agent target.
    let agent_after = if agent_before.is_empty() {
        String::new()
    } else {
        match curate_one_target(
            ai,
            subject_id,
            "agent",
            &agent_before,
            AGENT_MEMORY_CAP_CHARS,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    subject_id = %subject_id,
                    target = "agent",
                    error = %e,
                    "curator: agent target left unchanged"
                );
                agent_before.clone()
            }
        }
    };

    // User target. Run regardless of agent's outcome so one
    // bad target doesn't poison both.
    let user_after = if user_before.is_empty() {
        String::new()
    } else {
        match curate_one_target(ai, subject_id, "user", &user_before, USER_MEMORY_CAP_CHARS).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    subject_id = %subject_id,
                    target = "user",
                    error = %e,
                    "curator: user target left unchanged"
                );
                user_before.clone()
            }
        }
    };

    // Write back only if something actually changed.
    if agent_after != agent_before {
        store.agent_set_content(subject_id, "agent", &agent_after)?;
    }
    if user_after != user_before {
        store.agent_set_content(subject_id, "user", &user_after)?;
    }

    Ok(CuratorSubjectResult {
        subject_id: subject_id.to_string(),
        agent_entries_before,
        agent_entries_after: count_entries(&agent_after),
        agent_chars_before,
        agent_chars_after: agent_after.chars().count(),
        user_entries_before,
        user_entries_after: count_entries(&user_after),
        user_chars_before,
        user_chars_after: user_after.chars().count(),
    })
}

// ───────────────────────── Scheduler ───────────────────────────

/// Spawn the background curator task. Idempotent at the
/// caller level (controller runtime calls it at most once).
/// Silent-skips the entire run on any acquisition failure;
/// see crate-level docs for the "never wipe memory" contract.
pub fn spawn_curator_scheduler(
    store: Arc<MemoryStore>,
    state: Arc<Mutex<CuratorState>>,
    ai_cell: Arc<tokio::sync::OnceCell<Arc<dyn AiDispatcher>>>,
    cfg: CuratorConfig,
) {
    if !cfg.enabled {
        tracing::info!("memory curator: scheduler disabled by config");
        return;
    }
    let interval = Duration::from_secs(cfg.interval_secs.max(60));
    let min_chars = cfg.min_chars_to_curate;
    tokio::spawn(async move {
        // Initial warmup so the AI dispatcher discovery
        // (separate task) gets a chance to populate the
        // OnceCell before the first tick.
        tokio::time::sleep(Duration::from_secs(5)).await;
        let mut ticker = tokio::time::interval(interval);
        // Skip the immediate first tick — we already slept
        // for warmup.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            run_one_tick(&store, &state, &ai_cell, min_chars, interval.as_secs()).await;
        }
    });
}

/// One tick of the scheduler. Returns the summary it wrote
/// to the shared state. Visible to tests via the public path.
pub async fn run_one_tick(
    store: &MemoryStore,
    state: &Mutex<CuratorState>,
    ai_cell: &tokio::sync::OnceCell<Arc<dyn AiDispatcher>>,
    min_chars: usize,
    interval_secs: u64,
) -> CuratorRunSummary {
    // Concurrency guard.
    {
        let mut guard = state.lock().await;
        if guard.running {
            tracing::info!("memory curator: previous tick still in progress; skipping");
            return guard.last_run_summary.clone().unwrap_or_default();
        }
        guard.running = true;
        guard.last_run_at = Some(super::unix_secs());
    }

    let dispatcher = match ai_cell.get() {
        Some(d) => d.clone(),
        None => {
            tracing::warn!("memory curator: AI dispatcher not yet ready; skipping tick");
            let mut guard = state.lock().await;
            guard.running = false;
            guard.next_run_at = Some(super::unix_secs() + interval_secs as i64);
            return guard.last_run_summary.clone().unwrap_or_default();
        }
    };

    let subjects = match store.list_subjects_with_total_chars() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "memory curator: list_subjects failed");
            let mut guard = state.lock().await;
            guard.running = false;
            return guard.last_run_summary.clone().unwrap_or_default();
        }
    };

    let mut summary = CuratorRunSummary {
        agents_reviewed: subjects.len(),
        ..Default::default()
    };
    for (subject_id, total_chars) in subjects {
        if total_chars <= min_chars {
            continue;
        }
        match curate_subject(store, dispatcher.as_ref(), &subject_id).await {
            Ok(res) => {
                let saved = res.chars_saved();
                if saved > 0 || res.agent_entries_before > res.agent_entries_after {
                    summary.agents_curated += 1;
                    summary.total_chars_saved += saved;
                }
                tracing::info!(
                    subject_id = %subject_id,
                    agent_before = res.agent_entries_before,
                    agent_after = res.agent_entries_after,
                    user_before = res.user_entries_before,
                    user_after = res.user_entries_after,
                    chars_saved = saved,
                    "memory curator: agent reviewed"
                );
            }
            Err(e) => {
                tracing::warn!(
                    subject_id = %subject_id,
                    error = %e,
                    "memory curator: agent skipped (existing memory kept)"
                );
            }
        }
        // Avoid hammering the AI peer.
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    let mut guard = state.lock().await;
    guard.last_run_summary = Some(summary.clone());
    guard.next_run_at = Some(super::unix_secs() + interval_secs as i64);
    guard.running = false;
    summary
}

// ───────────────────────── Tests ───────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Canned `AiDispatcher` that returns a fixed reply (or
    /// `None` to simulate unavailability).
    struct StubAi {
        reply: Option<String>,
        calls: AtomicUsize,
    }

    impl StubAi {
        fn new(reply: Option<&str>) -> Self {
            Self {
                reply: reply.map(str::to_string),
                calls: AtomicUsize::new(0),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl AiDispatcher for StubAi {
        async fn chat(&self, _sid: &str, _prompt: &str, _hist: &str) -> Option<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.reply.clone()
        }
    }

    #[test]
    fn build_curation_prompt_contains_delimiter_cap_and_content() {
        let p = build_curation_prompt("alpha§beta", 2200);
        assert!(p.contains("§"));
        assert!(p.contains("2200"));
        assert!(p.contains("alpha§beta"));
        // Returns only the curated entries — make sure the
        // exact phrasing is preserved (operators rely on it).
        assert!(p.contains("Return ONLY the curated entries separated by §"));
    }

    #[test]
    fn count_entries_simple_cases() {
        assert_eq!(count_entries(""), 0);
        assert_eq!(count_entries("one"), 1);
        assert_eq!(count_entries("one§two"), 2);
        assert_eq!(count_entries("a§b§c"), 3);
    }

    #[tokio::test]
    async fn curate_subject_returns_empty_summary_when_both_targets_empty() {
        let store = MemoryStore::in_memory().unwrap();
        let ai = StubAi::new(Some("should not be called"));
        let r = curate_subject(&store, &ai, "alice").await.unwrap();
        assert_eq!(r.agent_chars_before, 0);
        assert_eq!(r.user_chars_before, 0);
        assert_eq!(r.chars_saved(), 0);
        // Crucially: no AI calls when both targets are empty.
        assert_eq!(ai.call_count(), 0);
    }

    #[tokio::test]
    async fn curate_subject_skips_empty_target_and_processes_other() {
        let store = MemoryStore::in_memory().unwrap();
        // Two separate add calls — `add` action forbids § in
        // the entry text (it's the delimiter); the store
        // joins them with § itself.
        store.agent_write("alice", "agent", "add", "alpha").unwrap();
        store.agent_write("alice", "agent", "add", "beta").unwrap();
        // alice's user target stays empty.
        let ai = StubAi::new(Some("alpha"));
        let r = curate_subject(&store, &ai, "alice").await.unwrap();
        // One AI call (agent target only — user is empty).
        assert_eq!(ai.call_count(), 1);
        assert_eq!(r.user_entries_before, 0);
        assert_eq!(r.user_entries_after, 0);
        assert!(r.agent_chars_after < r.agent_chars_before);
    }

    #[tokio::test]
    async fn curate_subject_writes_back_curated_content() {
        let store = MemoryStore::in_memory().unwrap();
        store.agent_write("alice", "agent", "add", "alpha").ok();
        store.agent_write("alice", "agent", "add", "beta").ok();
        let ai = StubAi::new(Some("alpha-and-beta"));
        let _ = curate_subject(&store, &ai, "alice").await.unwrap();
        let (agent, _) = store.agent_read("alice").unwrap();
        assert_eq!(agent, "alpha-and-beta");
    }

    #[tokio::test]
    async fn curate_subject_preserves_existing_on_ai_unavailable() {
        let store = MemoryStore::in_memory().unwrap();
        store.agent_write("alice", "agent", "add", "alpha").ok();
        store.agent_write("alice", "agent", "add", "beta").ok();
        let ai = StubAi::new(None); // unavailable
        let _ = curate_subject(&store, &ai, "alice").await.unwrap();
        let (agent, _) = store.agent_read("alice").unwrap();
        // Unchanged.
        assert_eq!(agent, "alpha§beta");
    }

    #[tokio::test]
    async fn curate_subject_rejects_empty_response_and_keeps_existing() {
        let store = MemoryStore::in_memory().unwrap();
        store.agent_write("alice", "agent", "add", "alpha").ok();
        let ai = StubAi::new(Some("   \n  ")); // whitespace-only
        let _ = curate_subject(&store, &ai, "alice").await.unwrap();
        let (agent, _) = store.agent_read("alice").unwrap();
        assert_eq!(agent, "alpha");
    }

    #[tokio::test]
    async fn curate_subject_rejects_over_cap_response_and_keeps_existing() {
        let store = MemoryStore::in_memory().unwrap();
        store.agent_write("alice", "agent", "add", "small").ok();
        // Stub returns an over-cap blob.
        let huge: String = std::iter::repeat_n('x', AGENT_MEMORY_CAP_CHARS + 50).collect();
        let ai = StubAi::new(Some(&huge));
        let _ = curate_subject(&store, &ai, "alice").await.unwrap();
        let (agent, _) = store.agent_read("alice").unwrap();
        assert_eq!(agent, "small");
    }

    #[tokio::test]
    async fn one_tick_skips_subjects_below_min_chars() {
        let store = Arc::new(MemoryStore::in_memory().unwrap());
        // Tiny: 5 chars total. min_chars = 100 → skipped.
        store.agent_write("alice", "agent", "add", "alpha").ok();
        // Larger: 200 chars total → curated.
        let big: String = std::iter::repeat_n('y', 200).collect();
        store.agent_write("bob", "agent", "add", &big).ok();

        let state = Arc::new(Mutex::new(CuratorState::default()));
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn AiDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let ai: Arc<dyn AiDispatcher> = Arc::new(StubAi::new(Some("short")));
        cell.set(ai).ok();

        let summary = run_one_tick(&store, &state, &cell, 100, 60).await;
        assert_eq!(summary.agents_reviewed, 2);
        // alice was below threshold; only bob curated.
        assert_eq!(summary.agents_curated, 1);
    }

    #[tokio::test]
    async fn one_tick_skips_when_ai_cell_empty() {
        let store = Arc::new(MemoryStore::in_memory().unwrap());
        let big: String = std::iter::repeat_n('y', 200).collect();
        store.agent_write("bob", "agent", "add", &big).ok();
        let state = Arc::new(Mutex::new(CuratorState::default()));
        let cell: Arc<tokio::sync::OnceCell<Arc<dyn AiDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        // Don't populate the cell.
        let summary = run_one_tick(&store, &state, &cell, 100, 60).await;
        // No curation happens.
        assert_eq!(summary.agents_curated, 0);
        // last_run_at is still recorded (tick fired even if it
        // bailed early).
        let guard = state.lock().await;
        assert!(guard.last_run_at.is_some());
    }

    #[tokio::test]
    async fn subject_result_wire_format_includes_all_fields() {
        let r = CuratorSubjectResult {
            subject_id: "alice".into(),
            agent_entries_before: 5,
            agent_entries_after: 3,
            agent_chars_before: 200,
            agent_chars_after: 120,
            user_entries_before: 3,
            user_entries_after: 2,
            user_chars_before: 80,
            user_chars_after: 50,
        };
        let w = r.to_wire();
        for needle in [
            "subject_id=alice",
            "agent_entries_before=5",
            "agent_entries_after=3",
            "agent_chars_before=200",
            "agent_chars_after=120",
            "user_entries_before=3",
            "user_entries_after=2",
            "user_chars_before=80",
            "user_chars_after=50",
            "chars_saved=110",
        ] {
            assert!(w.contains(needle), "wire body missing `{needle}`: {w}");
        }
    }
}
