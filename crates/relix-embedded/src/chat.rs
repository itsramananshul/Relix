//! Chat dispatch in embedded mode.
//!
//! The standalone Relix mesh routes a chat call through the dispatch
//! bridge (admission + audit), then the AI controller pulls history
//! from the memory peer via libp2p before calling the provider. In
//! embedded mode we own the memory store directly, so we can serve
//! both the history pull and the provider call without a single mesh
//! hop.
//!
//! History plumbing: `LayeredMemoryStore` does not expose a
//! "newest-N by `source`" public helper today, and the embedded
//! crate deliberately doesn't drop down to `Connection`-level SQL
//! (would require either a wider `LayeredMemoryStore` API or a
//! brittle reimplementation of its schema). Instead we keep a tiny
//! per-session ring buffer in process for the prompt's `history`
//! string AND we still persist every turn to the canonical store as
//! a Layer-1 `Raw` record so cross-process inspection + later
//! search keep working.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use relix_runtime::nodes::ai::provider::{ChatInput as ProviderChatInput, TokenUsage};

/// Local mirror of [`relix_runtime::nodes::ai::provider::TokenUsage`]
/// that derives `Serialize` + `Deserialize`. The upstream type is
/// usage-only and not serde-derived; mirroring locally keeps the
/// embedded crate's public surface fully serde-friendly without
/// dragging a serde dependency onto the runtime's provider trait.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReport {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl From<TokenUsage> for UsageReport {
    fn from(u: TokenUsage) -> Self {
        Self {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
        }
    }
}
use relix_runtime::nodes::memory::schema::{MemoryLayer, MemoryRecord};

use crate::{EmbeddedError, RelixEmbedded};

/// Max raw turns kept per session in the in-process history ring.
/// 20 matches the runtime's `memory.recent_for_session` default.
const HISTORY_MAX_TURNS: usize = 20;

/// Request body for [`RelixEmbedded::chat`](crate::RelixEmbedded::chat).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ChatInput {
    /// Session id. Used as the SQLite source key on the raw-turn
    /// rows AND as the key for the in-process history ring.
    pub session_id: String,
    /// The user's new message.
    pub message: String,
    /// Friendly agent name persisted alongside the turn — appears in
    /// chronicle and audit views.
    pub agent_name: String,
    /// Optional provider model id override. `None` (or an empty
    /// string) means "use the embedded runtime's `default_model`,
    /// which itself falls back to the provider's built-in default".
    pub model: Option<String>,
    /// Optional system-prompt override.
    pub system_prompt: Option<String>,
    /// PART 6 — tenant the persisted raw-turn rows are
    /// written under. `None` or empty falls back to the
    /// runtime's [`crate::RelixEmbedded::default_tenant_id`].
    /// When the runtime has `tenant_isolation = true` AND
    /// neither value resolves, the call returns
    /// [`crate::EmbeddedError::MissingTenant`]. The in-process
    /// history ring is keyed by `session_id` alone — tenant
    /// isolation is enforced at the persistence layer only.
    /// (Callers that need cross-tenant session isolation
    /// must namespace their `session_id` themselves; this
    /// matches the bridge surface where `session_id` is the
    /// caller-controlled string.)
    #[serde(default)]
    pub tenant_id: Option<String>,
}

/// Return value of [`RelixEmbedded::chat`](crate::RelixEmbedded::chat).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatResponse {
    /// Assistant's reply text.
    pub text: String,
    /// Provider that handled the call (`"mock"`, `"openai"`, etc.).
    pub provider: String,
    /// Resolved model id.
    pub model: String,
    /// Best-effort token accounting reported by the provider.
    pub usage: Option<UsageReport>,
}

/// Per-session conversation history. Bounded ring; thread-safe via a
/// `Mutex` because cheap clones of `RelixEmbedded` share one
/// `HistoryStore`.
///
/// CORR PART 3: `turn_seq` is a monotonic per-instance counter
/// used by [`turn_id`] to disambiguate turns. Pre-fix `turn_id`
/// salted with wall-clock nanos, which can go backwards across
/// NTP corrections AND repeats within a single nanosecond on
/// platforms with coarse-resolution clocks, producing colliding
/// memory ids. The atomic counter is strictly monotonic for the
/// life of the HistoryStore (which is the life of the
/// `RelixEmbedded` clone) so two turns can never share an id.
///
/// CORR PART 4: `sessions` is now an `LruCache` capped at
/// [`MAX_SESSIONS`] entries. Pre-fix `HashMap` grew without
/// limit on long-lived embedded deployments serving thousands
/// of distinct session ids (one new session per browser tab).
/// LRU eviction drops the least-recently-used session — its
/// history is lost, but the next call from that session id
/// just starts a fresh ring, which is the desired behaviour.
pub(crate) struct HistoryStore {
    sessions: Mutex<lru::LruCache<String, VecDeque<String>>>,
    turn_seq: std::sync::atomic::AtomicU64,
}

/// CORR PART 4: hard cap on the in-memory session history
/// table. 10000 is far more than the operator-supplied
/// embedded session count any sensible deployment runs.
pub const MAX_SESSIONS: usize = 10_000;

impl Default for HistoryStore {
    fn default() -> Self {
        let cap = std::num::NonZeroUsize::new(MAX_SESSIONS).expect("MAX_SESSIONS > 0");
        Self {
            sessions: Mutex::new(lru::LruCache::new(cap)),
            turn_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl HistoryStore {
    pub(crate) fn append(&self, session_id: &str, line: String) {
        let mut g = match self.sessions.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        // CORR PART 4: LRU semantics — get_or_insert_mut
        // returns a mutable handle, refreshing recency. When
        // the cap is hit, `put`-style inserts evict the LRU
        // entry; `get_or_insert_mut` does the same.
        let entry = g.get_or_insert_mut(session_id.to_string(), VecDeque::default);
        entry.push_back(line);
        while entry.len() > HISTORY_MAX_TURNS {
            entry.pop_front();
        }
    }

    pub(crate) fn render(&self, session_id: &str) -> String {
        let mut g = match self.sessions.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        // `get` bumps LRU recency.
        let Some(entry) = g.get(session_id) else {
            return String::new();
        };
        let mut out = String::new();
        for line in entry.iter() {
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }

    pub(crate) fn turns_for(&self, session_id: &str) -> usize {
        let mut g = match self.sessions.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        g.get(session_id).map_or(0, VecDeque::len)
    }

    /// CORR PART 3: monotonic per-instance counter used by
    /// [`turn_id`] to disambiguate two turns recorded inside
    /// the same wall-clock nanosecond or across an NTP
    /// correction. `SeqCst` keeps the order strict across
    /// concurrent appends from different threads.
    pub(crate) fn next_turn_seq(&self) -> u64 {
        self.turn_seq
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }
}

impl RelixEmbedded {
    /// Issue a chat call against the configured provider.
    ///
    /// The embedded dispatcher:
    /// 1. Renders the in-process conversation history for
    ///    `session_id` (bounded at 20 turns).
    /// 2. Sends a `ChatInput` to the provider with that history.
    /// 3. On success, persists BOTH the user turn AND the assistant
    ///    turn to the memory store as Layer-1 `Raw` records (best
    ///    effort — a memory write failure does not invalidate the
    ///    reply that already came back from the provider) AND
    ///    appends both turns to the in-process history ring so the
    ///    next call sees them.
    pub async fn chat(&self, input: ChatInput) -> Result<ChatResponse, EmbeddedError> {
        if input.session_id.trim().is_empty() {
            return Err(EmbeddedError::Config(
                "chat input requires a non-empty session_id".into(),
            ));
        }
        if input.message.is_empty() {
            return Err(EmbeddedError::Config(
                "chat input requires a non-empty message".into(),
            ));
        }
        // PART 6: resolve the effective tenant up-front so a
        // missing tenant in isolation mode fails BEFORE we
        // call the provider (no wasted API hit, no orphan
        // history-ring entries).
        let tenant_for_turns = self.resolve_tenant(input.tenant_id.as_deref(), "chat")?;

        let history = self.history_store_ref().render(&input.session_id);
        let model = match input.model.as_deref() {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => self.default_model().to_string(),
        };
        let provider_input = ProviderChatInput {
            session_id: input.session_id.clone(),
            prompt: input.message.clone(),
            history,
            model,
            system_prompt: input.system_prompt.clone(),
            temperature: None,
            max_tokens: None,
            thinking_budget_tokens: None,
        };

        let output = self
            .provider()
            .generate_reply(provider_input)
            .await
            .map_err(|e| EmbeddedError::Provider(e.to_string()))?;

        let agent_label = if input.agent_name.trim().is_empty() {
            "assistant".to_string()
        } else {
            input.agent_name.clone()
        };
        let user_line = format!("user: {}", input.message);
        let assistant_line = format!("{agent_label}: {}", output.text);

        // Persist + ring-append in lockstep so a future chat in the
        // same session sees consistent history. The persistence step
        // is best-effort because the in-process ring is the source
        // of truth for the next chat's history pull; if SQLite
        // write fails we log a warning but still return the reply.
        if let Err(e) = persist_turn(
            self,
            &input.session_id,
            &user_line,
            tenant_for_turns.as_deref(),
        ) {
            tracing::warn!(
                session_id = %input.session_id,
                error = %e,
                "embedded chat: failed to persist user turn (continuing)"
            );
        }
        if let Err(e) = persist_turn(
            self,
            &input.session_id,
            &assistant_line,
            tenant_for_turns.as_deref(),
        ) {
            tracing::warn!(
                session_id = %input.session_id,
                error = %e,
                "embedded chat: failed to persist assistant turn (continuing)"
            );
        }
        self.history_store_ref()
            .append(&input.session_id, user_line);
        self.history_store_ref()
            .append(&input.session_id, assistant_line);

        Ok(ChatResponse {
            text: output.text,
            provider: output.provider.to_string(),
            model: output.model,
            usage: output.usage.map(UsageReport::from),
        })
    }

    /// Return how many raw turns the in-process history ring holds
    /// for `session_id`. Useful for tests and operator visibility.
    pub fn session_turn_count(&self, session_id: &str) -> usize {
        self.history_store_ref().turns_for(session_id)
    }
}

fn persist_turn(
    runtime: &RelixEmbedded,
    session_id: &str,
    line: &str,
    tenant_id: Option<&str>,
) -> Result<(), EmbeddedError> {
    // CORR PART 3: salt the id with the HistoryStore's
    // monotonic counter so two turns in the same wall-clock
    // nanosecond cannot collide. Pre-fix path used
    // `SystemTime::now()` only, which is non-monotonic across
    // NTP corrections AND can repeat under coarse-resolution
    // clocks (Windows historically minted the same nanos value
    // for back-to-back calls).
    let seq = runtime.history_store_ref().next_turn_seq();
    let id = turn_id(session_id, line, seq);
    let mut record = MemoryRecord::new_raw(id, line, session_id);
    record.layer = MemoryLayer::Raw;
    // PART 6: stamp the resolved tenant so tenant-aware
    // memory reads (`text_search_for_tenant`,
    // `get_for_tenant`) only return rows belonging to the
    // querying tenant. `None` keeps the legacy untagged
    // behaviour for `tenant_isolation = false` callers.
    record.tenant_id = tenant_id.map(|s| s.to_string());
    runtime.memory_store().insert(&record)?;
    Ok(())
}

fn turn_id(session: &str, body: &str, seq: u64) -> String {
    let mut h = blake3::Hasher::new();
    h.update(session.as_bytes());
    h.update(b"|");
    h.update(body.as_bytes());
    h.update(b"|");
    // CORR PART 3: salt with the HistoryStore's monotonic
    // counter (caller-provided) instead of wall-clock nanos.
    // Strictly monotonic for the life of the HistoryStore;
    // unaffected by NTP corrections or platform clock
    // resolution.
    h.update(&seq.to_le_bytes());
    let hex = h.finalize().to_hex();
    format!("raw-{}", &hex.as_str()[..16])
}
