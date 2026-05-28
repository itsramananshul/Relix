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
    /// Optional outbound peer pointing at the Coordinator
    /// node. When set, the curator writes a
    /// `memory.curator_run` chronicle event after every
    /// scheduler tick. When absent, the chronicle event is
    /// skipped (logged at WARN once per tick); the curator
    /// otherwise runs normally.
    #[serde(default, rename = "coord_peer")]
    pub coord_peer: Option<CoordPeerConfig>,
    /// Master switch for the four-layer promotion loop
    /// (`promoter.rs`). Distinct from `enabled`, which controls
    /// the per-subject agent/user memory consolidation
    /// scheduler. Default `false` so existing deployments keep
    /// their current behaviour exactly.
    #[serde(default)]
    pub promotion_enabled: bool,
    /// Seconds between promotion ticks. Default 300 (5 min).
    /// Each tick runs Raw → Semantic, then Semantic →
    /// Observation, then Observation → Model in sequence.
    #[serde(default = "default_promotion_interval_secs")]
    pub promotion_interval_secs: u64,
    /// Maximum records each promotion stage processes per
    /// tick. Default 20.
    #[serde(default = "default_promotion_batch_size")]
    pub promotion_batch_size: usize,
    /// Model identifier the `memory.dialectic` capability
    /// reports on the `model_used` field of its response.
    /// Defaults to the spec's recommended cheap-tier model.
    #[serde(default = "default_dialectic_model")]
    pub dialectic_model: String,
}

fn default_promotion_interval_secs() -> u64 {
    300
}

fn default_promotion_batch_size() -> usize {
    20
}

fn default_dialectic_model() -> String {
    super::dialectic::DEFAULT_DIALECTIC_MODEL.to_string()
}

impl Default for CuratorConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            interval_secs: default_interval_secs(),
            min_chars_to_curate: default_min_chars(),
            ai_peer: None,
            coord_peer: None,
            promotion_enabled: false,
            promotion_interval_secs: default_promotion_interval_secs(),
            promotion_batch_size: default_promotion_batch_size(),
            dialectic_model: default_dialectic_model(),
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

/// `[memory.curator.coord_peer]` — optional outbound peer
/// pointing at a Coordinator node. When set, the curator
/// writes a `memory.curator_run` chronicle event after every
/// scheduler tick. When absent, the chronicle event is
/// skipped (with a one-line WARN); the curator otherwise
/// runs normally.
#[derive(Clone, Debug, Deserialize)]
pub struct CoordPeerConfig {
    /// libp2p multiaddr (e.g. `/ip4/127.0.0.1/tcp/19714`).
    pub addr: String,
    /// Alias the outbound MeshClient uses to dial. Defaults
    /// to `"coordinator"`.
    #[serde(default = "default_coord_alias")]
    pub alias: String,
    /// Per-call deadline in seconds. `task.event` is a cheap
    /// SQLite insert; 10s is plenty.
    #[serde(default = "default_coord_deadline_secs")]
    pub deadline_secs: i64,
}

fn default_coord_alias() -> String {
    "coordinator".to_string()
}

fn default_coord_deadline_secs() -> i64 {
    10
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

// ───────────────────────── EmbeddingDispatcher ────────────────

/// Errors a [`EmbeddingDispatcher`] can surface. Kept narrow on
/// purpose — the memory.embed / memory.search handlers map every
/// variant to `RESPONDER_INTERNAL`.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("not connected: embedding dispatcher not wired (no [memory.embedding_peer])")]
    NotConnected,
    #[error("transport: {0}")]
    Transport(String),
    #[error("decode: {0}")]
    Decode(String),
    #[error("responder: {0}")]
    Responder(String),
}

/// Async hook the memory node reaches through to call `ai.embed`
/// on a configured embedding peer. Production wraps a
/// `MeshClient`; tests stub it. Modelled on [`AiDispatcher`].
#[async_trait]
pub trait EmbeddingDispatcher: Send + Sync {
    /// Generate embeddings for a batch of texts. Returns one
    /// vector per input text in the same order; vectors are not
    /// guaranteed to be unit-length (cosine similarity
    /// normalises).
    async fn embed(&self, model: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError>;
}

/// Live `EmbeddingDispatcher` — wraps a `MeshClient` pointing at
/// the AI peer that exposes `ai.embed`. Constructed by the
/// memory controller at startup once
/// `[memory.embedding_peer]` is parsed and the peer dialled.
#[derive(Clone)]
pub struct EmbeddingMeshDispatcher {
    mesh: MeshClient,
    alias: String,
    identity: Bundle,
    deadline_secs: i64,
}

impl EmbeddingMeshDispatcher {
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
impl EmbeddingDispatcher for EmbeddingMeshDispatcher {
    async fn embed(&self, model: &str, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        use base64::Engine;
        // Wire: arg `model|text1§text2§…`. Each text must not
        // contain `§` (the responder splits on it); the caller is
        // expected to have stripped or escaped them upstream.
        let mut arg =
            String::with_capacity(model.len() + texts.iter().map(|t| t.len() + 1).sum::<usize>());
        arg.push_str(model);
        arg.push('|');
        for (i, t) in texts.iter().enumerate() {
            if i > 0 {
                arg.push('§');
            }
            arg.push_str(t);
        }
        let envelope = build_request(
            "ai.embed",
            arg.into_bytes(),
            self.identity.clone(),
            self.deadline_secs,
        );
        let resp_bytes = self
            .mesh
            .call(&self.alias, envelope)
            .await
            .map_err(|e| EmbeddingError::Transport(format!("call: {e}")))?;
        let resp = decode_response(&resp_bytes)
            .map_err(|e| EmbeddingError::Decode(format!("decode: {e}")))?;
        let body = match resp.res {
            ResponseResult::Ok(b) => b.to_vec(),
            ResponseResult::Err(env) => {
                return Err(EmbeddingError::Responder(format!(
                    "kind={} cause={}",
                    env.kind, env.cause
                )));
            }
            ResponseResult::StreamHandle(_) => {
                return Err(EmbeddingError::Decode(
                    "unexpected stream response from ai.embed".into(),
                ));
            }
        };
        let text =
            String::from_utf8(body).map_err(|e| EmbeddingError::Decode(format!("utf8: {e}")))?;
        // Response: `model|b64(vec_1)|b64(vec_2)|…\n`. We don't
        // need the model on this side — the call site supplied
        // it.
        let trimmed = text.trim_end_matches('\n');
        let mut parts = trimmed.split('|');
        let _model = parts.next().unwrap_or_default();
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for (i, p) in parts.enumerate() {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(p.as_bytes())
                .map_err(|e| EmbeddingError::Decode(format!("vector {i}: base64: {e}")))?;
            if bytes.len() % 4 != 0 {
                return Err(EmbeddingError::Decode(format!(
                    "vector {i}: byte length {} not a multiple of 4",
                    bytes.len()
                )));
            }
            let mut v = Vec::with_capacity(bytes.len() / 4);
            for chunk in bytes.chunks_exact(4) {
                v.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
            out.push(v);
        }
        if out.len() != texts.len() {
            return Err(EmbeddingError::Decode(format!(
                "expected {} vectors, got {}",
                texts.len(),
                out.len()
            )));
        }
        Ok(out)
    }
}

// ───────────────────────── CoordDispatcher ────────────────

/// Async hook the curator reaches through to write a
/// `memory.curator_run` chronicle event to a Coordinator
/// peer. Production wraps a `MeshClient`; tests stub it.
///
/// Two methods:
///
/// - `ensure_system_task` — get-or-create the synthetic
///   "memory curator system" task and return its task_id.
///   Cached by the scheduler.
/// - `append_event` — call `task.event` on an existing
///   task with the run summary as payload.
///
/// Both return `None` on any failure (network, decode,
/// responder err). The scheduler logs a WARN and continues
/// — chronicle writes are best-effort.
#[async_trait]
pub trait CoordDispatcher: Send + Sync {
    /// Get-or-create the synthetic system task that holds
    /// chronicle entries for curator runs. Returns the
    /// task_id on success, `None` on failure.
    async fn ensure_system_task(&self) -> Option<String>;

    /// Append a `memory.curator_run` event to `task_id` with
    /// a pipe-delim payload encoding the run summary. Returns
    /// `true` on success.
    async fn append_curator_event(&self, task_id: &str, summary: &CuratorRunSummary) -> bool;

    /// Proxy to the coordinator's `task.session_search`
    /// capability. Returns the JSON body verbatim on success
    /// or `Err(human-readable)` on transport / responder
    /// failure so callers can surface the cause rather than
    /// turning it into a silent skip.
    async fn session_search(
        &self,
        subject_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<String, String>;
}

/// Live `CoordDispatcher` implementation — wraps a
/// `MeshClient` pointing at the coordinator peer.
#[derive(Clone)]
pub struct CoordMeshDispatcher {
    mesh: MeshClient,
    alias: String,
    identity: Bundle,
    deadline_secs: i64,
}

impl CoordMeshDispatcher {
    pub fn new(mesh: MeshClient, alias: String, identity: Bundle, deadline_secs: i64) -> Self {
        Self {
            mesh,
            alias,
            identity,
            deadline_secs,
        }
    }

    /// Title used for the system task. Searched for at
    /// `ensure_system_task` time — when found, reused;
    /// when missing, created.
    const SYSTEM_TASK_TITLE: &'static str = "memory-curator-system";
    const SYSTEM_TASK_FLOW: &'static str = "system:memory-curator";

    async fn call(&self, method: &str, arg: Vec<u8>) -> Option<Vec<u8>> {
        let envelope = build_request(method, arg, self.identity.clone(), self.deadline_secs);
        let resp_bytes = match self.mesh.call(&self.alias, envelope).await {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(
                    alias = %self.alias,
                    method = %method,
                    error = %e,
                    "curator coord call failed"
                );
                return None;
            }
        };
        let resp = decode_response(&resp_bytes).ok()?;
        match resp.res {
            ResponseResult::Ok(body) => Some(body.to_vec()),
            ResponseResult::Err(env) => {
                tracing::debug!(
                    alias = %self.alias,
                    method = %method,
                    cause = %env.cause,
                    "curator coord err response"
                );
                None
            }
            ResponseResult::StreamHandle(_) => None,
        }
    }

    /// Look for an existing system task by walking
    /// `task.list` (paged). The coordinator doesn't expose a
    /// title-search capability, so we paginate up to a
    /// reasonable bound. Returns `None` if not found within
    /// the bound.
    async fn find_existing_system_task(&self) -> Option<String> {
        // Page size 200, scan up to 5 pages (1000 tasks). Any
        // real deployment with more than 1k tasks above the
        // system task should restart-create a new one rather
        // than waste a lot of scan budget.
        for page in 0..5 {
            let offset = page * 200;
            let arg = format!("200|{offset}|");
            let body = self.call("task.list", arg.into_bytes()).await?;
            let text = String::from_utf8(body).ok()?;
            // task.list rows are tab-delimited; the first column
            // is task_id, then title. We just scan for our
            // sentinel title. (The exact column layout is
            // documented in coordinator/mod.rs; here we keep
            // the parse forgiving — split each line on \t and
            // look for an exact title match in any column.)
            for line in text.lines() {
                if line.starts_with("count=") || line.trim().is_empty() {
                    continue;
                }
                let cols: Vec<&str> = line.split('\t').collect();
                if cols.contains(&Self::SYSTEM_TASK_TITLE) {
                    let task_id = cols.first().copied().unwrap_or("");
                    if !task_id.is_empty() {
                        return Some(task_id.to_string());
                    }
                }
            }
            if !text.contains('\n') {
                break;
            }
        }
        None
    }
}

#[async_trait]
impl CoordDispatcher for CoordMeshDispatcher {
    async fn ensure_system_task(&self) -> Option<String> {
        if let Some(id) = self.find_existing_system_task().await {
            tracing::debug!(task_id = %id, "curator: reusing existing system task");
            return Some(id);
        }
        // Create. `title|flow_template` is the minimum.
        let arg = format!("{}|{}", Self::SYSTEM_TASK_TITLE, Self::SYSTEM_TASK_FLOW);
        let body = self.call("task.create", arg.into_bytes()).await?;
        let id = String::from_utf8(body).ok()?.trim().to_string();
        if id.is_empty() {
            return None;
        }
        tracing::info!(task_id = %id, "curator: created system task for chronicle events");
        Some(id)
    }

    async fn append_curator_event(&self, task_id: &str, summary: &CuratorRunSummary) -> bool {
        let payload = format!(
            "agents_reviewed={}|agents_curated={}|total_chars_saved={}",
            summary.agents_reviewed, summary.agents_curated, summary.total_chars_saved,
        );
        let arg = format!("{task_id}|memory.curator_run|{payload}");
        self.call("task.event", arg.into_bytes()).await.is_some()
    }

    async fn session_search(
        &self,
        subject_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<String, String> {
        let arg = format!("{subject_id}|{query}|{limit}");
        let envelope = build_request(
            "task.session_search",
            arg.into_bytes(),
            self.identity.clone(),
            self.deadline_secs,
        );
        let resp_bytes = self
            .mesh
            .call(&self.alias, envelope)
            .await
            .map_err(|e| format!("coord transport: {e}"))?;
        let resp = decode_response(&resp_bytes).map_err(|e| format!("decode coord resp: {e}"))?;
        match resp.res {
            ResponseResult::Ok(body) => {
                String::from_utf8(body.to_vec()).map_err(|e| format!("coord body utf8: {e}"))
            }
            ResponseResult::Err(env) => Err(format!(
                "coord task.session_search err kind={} cause={}",
                env.kind, env.cause
            )),
            ResponseResult::StreamHandle(_) => {
                Err("coord task.session_search returned unexpected stream handle".to_string())
            }
        }
    }
}

// ───────────────────────── State ───────────────────────────────

/// In-memory status shared between the scheduler, the
/// `memory.agent_curate` handler, the `memory.curator_status`
/// handler, and the bridge's `/v1/memory/curator/status`
/// proxy.
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
    /// Cached system task_id used as the chronicle target for
    /// `memory.curator_run` events. Created lazily on the
    /// first successful tick when a coord_peer is configured;
    /// reused for every subsequent tick within the process.
    /// `None` means the chronicle write hasn't happened yet.
    pub system_task_id: Option<String>,
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
    coord_cell: Arc<tokio::sync::OnceCell<Arc<dyn CoordDispatcher>>>,
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
            run_one_tick(
                &store,
                &state,
                &ai_cell,
                &coord_cell,
                min_chars,
                interval.as_secs(),
            )
            .await;
        }
    });
}

/// One tick of the scheduler. Returns the summary it wrote
/// to the shared state. Visible to tests via the public path.
pub async fn run_one_tick(
    store: &MemoryStore,
    state: &Mutex<CuratorState>,
    ai_cell: &tokio::sync::OnceCell<Arc<dyn AiDispatcher>>,
    coord_cell: &tokio::sync::OnceCell<Arc<dyn CoordDispatcher>>,
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

    // Persist run telemetry into the shared state BEFORE
    // attempting the chronicle write, so a missing coord
    // peer doesn't hide the in-process status. Pull the
    // cached system_task_id while we hold the lock.
    let cached_system_task_id = {
        let mut guard = state.lock().await;
        guard.last_run_summary = Some(summary.clone());
        guard.next_run_at = Some(super::unix_secs() + interval_secs as i64);
        guard.running = false;
        guard.system_task_id.clone()
    };

    // Best-effort chronicle write. Get-or-create the system
    // task on first call, then append a `memory.curator_run`
    // event with the run counters as the payload.
    if let Some(coord) = coord_cell.get() {
        let task_id = if let Some(id) = cached_system_task_id {
            Some(id)
        } else {
            let id = coord.ensure_system_task().await;
            if let Some(id_str) = id.as_ref() {
                // Persist the cache so subsequent ticks skip
                // the list-and-create dance.
                state.lock().await.system_task_id = Some(id_str.clone());
            }
            id
        };
        match task_id {
            Some(id) => {
                if !coord.append_curator_event(&id, &summary).await {
                    tracing::warn!(
                        task_id = %id,
                        "memory curator: chronicle write failed (continuing — curator state still recorded in-process)"
                    );
                }
            }
            None => {
                tracing::warn!(
                    "memory curator: could not get-or-create system task; skipping chronicle event"
                );
            }
        }
    } else {
        tracing::warn!("memory curator: coord dispatcher not configured; skipping chronicle event");
    }

    summary
}

// ───────────────────────── memory.curator_status ───────────────

/// Render the curator's live state as the wire body for
/// `memory.curator_status`. Pipe-delimited key=value pairs on
/// one line so operators can parse with a simple
/// `split('|')`. Missing optional values render as `-1` for
/// timestamps and `0` for counters so the consumer never has
/// to handle `null`.
pub fn render_status_body(state: &CuratorState, cfg: &CuratorConfig) -> String {
    let last_run_at = state.last_run_at.unwrap_or(-1);
    let next_run_at = state.next_run_at.unwrap_or(-1);
    let (reviewed, curated, saved) = match &state.last_run_summary {
        Some(s) => (s.agents_reviewed, s.agents_curated, s.total_chars_saved),
        None => (0, 0, 0),
    };
    format!(
        "enabled={}|interval_secs={}|min_chars_to_curate={}|running={}|last_run_at={}|next_run_at={}|last_agents_reviewed={}|last_agents_curated={}|last_total_chars_saved={}\n",
        cfg.enabled,
        cfg.interval_secs,
        cfg.min_chars_to_curate,
        state.running,
        last_run_at,
        next_run_at,
        reviewed,
        curated,
        saved,
    )
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

        let coord_cell: Arc<tokio::sync::OnceCell<Arc<dyn CoordDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let summary = run_one_tick(&store, &state, &cell, &coord_cell, 100, 60).await;
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
        let coord_cell: Arc<tokio::sync::OnceCell<Arc<dyn CoordDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let summary = run_one_tick(&store, &state, &cell, &coord_cell, 100, 60).await;
        // No curation happens.
        assert_eq!(summary.agents_curated, 0);
        // last_run_at is still recorded (tick fired even if it
        // bailed early).
        let guard = state.lock().await;
        assert!(guard.last_run_at.is_some());
    }

    // ── Status capability + chronicle write ──────────────────

    #[test]
    fn render_status_body_shape_matches_spec() {
        let state = CuratorState {
            last_run_at: Some(1716000000),
            next_run_at: Some(1716003600),
            last_run_summary: Some(CuratorRunSummary {
                agents_reviewed: 5,
                agents_curated: 3,
                total_chars_saved: 120,
            }),
            ..Default::default()
        };
        let cfg = CuratorConfig {
            enabled: true,
            interval_secs: 3600,
            min_chars_to_curate: 100,
            ai_peer: None,
            coord_peer: None,
            promotion_enabled: false,
            promotion_interval_secs: 300,
            promotion_batch_size: 20,
            dialectic_model: default_dialectic_model(),
        };
        let body = render_status_body(&state, &cfg);
        for needle in [
            "enabled=true",
            "interval_secs=3600",
            "min_chars_to_curate=100",
            "running=false",
            "last_run_at=1716000000",
            "next_run_at=1716003600",
            "last_agents_reviewed=5",
            "last_agents_curated=3",
            "last_total_chars_saved=120",
        ] {
            assert!(
                body.contains(needle),
                "status body missing {needle}: {body}"
            );
        }
    }

    #[test]
    fn render_status_body_missing_run_uses_sentinels() {
        let state = CuratorState::default();
        let cfg = CuratorConfig::default();
        let body = render_status_body(&state, &cfg);
        // Timestamps render as -1; counters as 0.
        assert!(body.contains("last_run_at=-1"));
        assert!(body.contains("next_run_at=-1"));
        assert!(body.contains("last_agents_reviewed=0"));
        assert!(body.contains("last_agents_curated=0"));
        assert!(body.contains("last_total_chars_saved=0"));
    }

    /// CoordDispatcher stub for chronicle-write tests.
    struct StubCoord {
        ensure_calls: AtomicUsize,
        append_calls: AtomicUsize,
        return_task_id: Option<String>,
        appended_summaries: std::sync::Mutex<Vec<CuratorRunSummary>>,
    }

    impl StubCoord {
        fn new(task_id: Option<&str>) -> Self {
            Self {
                ensure_calls: AtomicUsize::new(0),
                append_calls: AtomicUsize::new(0),
                return_task_id: task_id.map(str::to_string),
                appended_summaries: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl CoordDispatcher for StubCoord {
        async fn ensure_system_task(&self) -> Option<String> {
            self.ensure_calls.fetch_add(1, Ordering::SeqCst);
            self.return_task_id.clone()
        }
        async fn append_curator_event(&self, _task_id: &str, summary: &CuratorRunSummary) -> bool {
            self.append_calls.fetch_add(1, Ordering::SeqCst);
            self.appended_summaries
                .lock()
                .unwrap()
                .push(summary.clone());
            true
        }
        async fn session_search(
            &self,
            _subject_id: &str,
            _query: &str,
            _limit: usize,
        ) -> Result<String, String> {
            // Tests that exercise session_search use a dedicated
            // stub; the chronicle-write tests never call this.
            Err("session_search not implemented in StubCoord".to_string())
        }
    }

    #[tokio::test]
    async fn tick_writes_curator_run_event_with_real_summary() {
        let store = Arc::new(MemoryStore::in_memory().unwrap());
        let big: String = std::iter::repeat_n('y', 200).collect();
        store.agent_write("bob", "agent", "add", &big).ok();
        let state = Arc::new(Mutex::new(CuratorState::default()));
        let ai_cell: Arc<tokio::sync::OnceCell<Arc<dyn AiDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let ai: Arc<dyn AiDispatcher> = Arc::new(StubAi::new(Some("short")));
        ai_cell.set(ai).ok();
        let coord_cell: Arc<tokio::sync::OnceCell<Arc<dyn CoordDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let coord = Arc::new(StubCoord::new(Some("00000000000000000000000000000001")));
        coord_cell
            .set(coord.clone() as Arc<dyn CoordDispatcher>)
            .ok();
        let summary = run_one_tick(&store, &state, &ai_cell, &coord_cell, 100, 60).await;
        assert_eq!(summary.agents_reviewed, 1);
        // ensure_system_task fired once; append_curator_event
        // fired once with the matching summary.
        assert_eq!(coord.ensure_calls.load(Ordering::SeqCst), 1);
        assert_eq!(coord.append_calls.load(Ordering::SeqCst), 1);
        let recorded = coord.appended_summaries.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].agents_reviewed, summary.agents_reviewed);
        assert_eq!(recorded[0].agents_curated, summary.agents_curated);
        assert_eq!(recorded[0].total_chars_saved, summary.total_chars_saved);
        // Cached task_id stored on state so subsequent ticks
        // skip ensure_system_task.
        let g = state.lock().await;
        assert_eq!(
            g.system_task_id.as_deref(),
            Some("00000000000000000000000000000001"),
        );
    }

    #[tokio::test]
    async fn tick_skips_chronicle_when_coord_unset_but_keeps_running() {
        let store = Arc::new(MemoryStore::in_memory().unwrap());
        let big: String = std::iter::repeat_n('y', 200).collect();
        store.agent_write("bob", "agent", "add", &big).ok();
        let state = Arc::new(Mutex::new(CuratorState::default()));
        let ai_cell: Arc<tokio::sync::OnceCell<Arc<dyn AiDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let ai: Arc<dyn AiDispatcher> = Arc::new(StubAi::new(Some("short")));
        ai_cell.set(ai).ok();
        let coord_cell: Arc<tokio::sync::OnceCell<Arc<dyn CoordDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        // Coord cell intentionally empty.
        let summary = run_one_tick(&store, &state, &ai_cell, &coord_cell, 100, 60).await;
        assert_eq!(summary.agents_reviewed, 1);
        // Run still records into state.
        let g = state.lock().await;
        assert!(g.last_run_at.is_some());
        assert!(g.last_run_summary.is_some());
        assert!(g.system_task_id.is_none());
    }

    #[tokio::test]
    async fn tick_caches_system_task_id_across_calls() {
        let store = Arc::new(MemoryStore::in_memory().unwrap());
        let big: String = std::iter::repeat_n('y', 200).collect();
        store.agent_write("bob", "agent", "add", &big).ok();
        let state = Arc::new(Mutex::new(CuratorState::default()));
        let ai_cell: Arc<tokio::sync::OnceCell<Arc<dyn AiDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let ai: Arc<dyn AiDispatcher> = Arc::new(StubAi::new(Some("short")));
        ai_cell.set(ai).ok();
        let coord_cell: Arc<tokio::sync::OnceCell<Arc<dyn CoordDispatcher>>> =
            Arc::new(tokio::sync::OnceCell::new());
        let coord = Arc::new(StubCoord::new(Some("aaaa1111aaaa1111aaaa1111aaaa1111")));
        coord_cell
            .set(coord.clone() as Arc<dyn CoordDispatcher>)
            .ok();
        // First tick — ensure_system_task fires.
        let _ = run_one_tick(&store, &state, &ai_cell, &coord_cell, 100, 60).await;
        // Second tick — uses cached id, ensure_system_task
        // should NOT fire again.
        let _ = run_one_tick(&store, &state, &ai_cell, &coord_cell, 100, 60).await;
        assert_eq!(coord.ensure_calls.load(Ordering::SeqCst), 1);
        assert_eq!(coord.append_calls.load(Ordering::SeqCst), 2);
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
