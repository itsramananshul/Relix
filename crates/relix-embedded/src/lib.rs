//! # relix-embedded
//!
//! Run a subset of the Relix platform in-process — chat against a real
//! AI provider plus the full four-layer memory system — without
//! standing up the libp2p mesh, the web bridge, or the CLI. This crate
//! is for developers who want Relix capabilities embedded directly
//! inside their own Rust application.
//!
//! ## What's included
//!
//! - Memory node — the same SQLite-backed
//!   [`relix_runtime::nodes::memory::schema::LayeredMemoryStore`]
//!   used by the standalone memory node. Chunked document ingest,
//!   text search (SQLite `LIKE`), and direct record CRUD.
//! - AI node — any
//!   [`relix_runtime::nodes::ai::provider::ChatProvider`] implementation
//!   (Mock, OpenAI-compatible / Ollama, Anthropic, Gemini) plugs in
//!   directly. Chat requests bypass libp2p — the embedded dispatcher
//!   calls the provider in-process and writes the raw turn into the
//!   memory store.
//! - Builder-pattern bootstrap. One async call boots everything; the
//!   resulting [`RelixEmbedded`] is `Clone` so callers can share it
//!   across tasks cheaply.
//!
//! ## What's NOT included
//!
//! - libp2p mesh networking. There is no peer discovery and no
//!   remote-peer dispatch.
//! - The web bridge HTTP server (use the `relix-web-bridge` binary if
//!   you need it).
//! - The CLI.
//! - Multi-node federation (no coordinator persistence; the embedded
//!   `chat` is fire-and-forget plus the memory turn write).
//!
//! That set is intentional — the value of embedded mode is that the
//! caller's app stays the only running process. When the app needs
//! cross-process orchestration it should run the full mesh instead.
//!
//! ## Usage
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use relix_embedded::{ChatInput, MemoryIngestInput, MemorySearchInput, RelixEmbedded};
//! use relix_runtime::nodes::ai::provider::MockProvider;
//!
//! # async fn run() -> Result<(), relix_embedded::EmbeddedError> {
//! let relix = RelixEmbedded::builder()
//!     .provider(Arc::new(MockProvider))
//!     .memory_db("./relix-memory.db")
//!     .build()
//!     .await?;
//!
//! // Chat with the configured provider.
//! let response = relix
//!     .chat(ChatInput {
//!         session_id: "user-123".into(),
//!         message: "Tell me about pricing".into(),
//!         agent_name: "assistant".into(),
//!         model: None,
//!         system_prompt: None,
//!         tenant_id: None,
//!     })
//!     .await?;
//! println!("{}", response.text);
//!
//! // Ingest a document so future questions can reference it.
//! relix
//!     .memory_ingest_document(MemoryIngestInput {
//!         subject_id: "user-123".into(),
//!         content: "Pricing tier B is $99/mo".into(),
//!         content_type: "markdown".into(),
//!         source: "notes.md".into(),
//!         tenant_id: None,
//!     })
//!     .await?;
//!
//! // Search memory.
//! let hits = relix
//!     .memory_search(MemorySearchInput {
//!         query: "pricing".into(),
//!         subject_id: "user-123".into(),
//!         limit: 5,
//!         tenant_id: None,
//!     })
//!     .await?;
//! for hit in hits {
//!     println!("{}: {}", hit.layer, hit.text);
//! }
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

mod chat;
mod memory;

pub use chat::{ChatInput, ChatResponse};
pub use memory::{MemoryHit, MemoryIngestInput, MemoryIngestResult, MemorySearchInput};

use std::path::PathBuf;
use std::sync::Arc;

use relix_runtime::nodes::ai::provider::ChatProvider;
use relix_runtime::nodes::memory::schema::{LayeredMemoryError, LayeredMemoryStore};

use crate::chat::HistoryStore;

/// Error class for every `RelixEmbedded` operation.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddedError {
    /// Builder rejected its inputs (no provider, invalid path, etc.).
    #[error("invalid config: {0}")]
    Config(String),
    /// Memory layer surfaced an error (db open, lock, serialisation).
    #[error("memory: {0}")]
    Memory(#[from] LayeredMemoryError),
    /// AI provider returned an error.
    #[error("provider: {0}")]
    Provider(String),
    /// Ingest chunker rejected the input (empty body, unsupported
    /// content type).
    #[error("ingest: {0}")]
    Ingest(String),
    /// PART 6 — `tenant_isolation` is enabled on the embedded
    /// runtime AND no tenant id was supplied (per-call input
    /// nor [`RelixEmbedded::default_tenant_id`]). Returned by
    /// `chat`, `memory_search`, `memory_ingest_document`.
    /// Operators get a precise error message naming the
    /// operation that hit the gate.
    #[error("tenant: tenant_id required for {op} in multi-tenant mode")]
    MissingTenant {
        /// Name of the embedded operation that triggered the
        /// gate (`"chat"`, `"memory_search"`, etc.).
        op: &'static str,
    },
}

/// The in-process Relix runtime. Cheap to clone — `LayeredMemoryStore`
/// and the provider Arc share storage and connection pools, so every
/// clone points at the same memory.
///
/// Constructed via [`RelixEmbedded::builder()`]. The builder is the
/// only public surface for boot — direct field construction is
/// deliberately not supported so the embedded crate retains the
/// option to grow new bootstrap responsibilities (cache warmup,
/// schema migration probes, etc.) without a breaking API change.
#[derive(Clone)]
pub struct RelixEmbedded {
    memory: LayeredMemoryStore,
    provider: Arc<dyn ChatProvider>,
    default_model: String,
    chunk_size_chars: usize,
    history: Arc<HistoryStore>,
    /// PART 6 — operator-supplied default tenant id. When a
    /// per-call `ChatInput::tenant_id` /
    /// `MemorySearchInput::tenant_id` /
    /// `MemoryIngestInput::tenant_id` is `None`, this value
    /// is used as the effective tenant. `None` means "no
    /// default" — when `tenant_isolation = true`, the
    /// per-call input MUST supply one or the operation
    /// returns [`EmbeddedError::MissingTenant`].
    default_tenant_id: Option<String>,
    /// PART 6 — fail-closed mode. When `true`, every
    /// operation that lacks a resolvable tenant id returns
    /// `MissingTenant` rather than silently filing into the
    /// no-tenant bucket. When `false`, missing tenant ids
    /// pass through with `None` (pre-PART-6 behaviour).
    /// The underlying [`LayeredMemoryStore`] is also opened
    /// with the same flag so the SQLite fallback path
    /// applies `WHERE tenant_id = ?` filtering.
    tenant_isolation: bool,
}

impl RelixEmbedded {
    /// Start the builder.
    pub fn builder() -> RelixEmbeddedBuilder {
        RelixEmbeddedBuilder::default()
    }

    /// Borrow the underlying memory store. Useful when the host app
    /// wants to issue lower-level queries (e.g. `text_search`,
    /// `invalidate`) than this crate's [`memory_search`] /
    /// [`memory_ingest_document`] wrappers expose.
    ///
    /// [`memory_search`]: Self::memory_search
    /// [`memory_ingest_document`]: Self::memory_ingest_document
    pub fn memory_store(&self) -> &LayeredMemoryStore {
        &self.memory
    }

    /// Borrow the underlying provider. Useful when the host app needs
    /// to issue lower-level provider calls (embeddings, streaming).
    pub fn provider(&self) -> &Arc<dyn ChatProvider> {
        &self.provider
    }

    /// Default model id this embedded runtime sends when
    /// [`ChatInput::model`] is unset. Empty string means "let the
    /// provider pick its built-in default".
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    pub(crate) fn chunk_size_chars(&self) -> usize {
        self.chunk_size_chars
    }

    /// Internal: shared handle on the per-session conversation
    /// history ring. Used by [`Self::chat`].
    pub(crate) fn history_store_ref(&self) -> &Arc<HistoryStore> {
        &self.history
    }

    /// PART 6 — operator-configured default tenant id. `None`
    /// when no default was set; in that case the per-call
    /// input MUST supply a tenant id when
    /// [`Self::tenant_isolation_enabled`] is `true`.
    pub fn default_tenant_id(&self) -> Option<&str> {
        self.default_tenant_id.as_deref()
    }

    /// PART 6 — `true` when fail-closed tenant isolation is
    /// enabled. Operations that resolve to no tenant id
    /// return [`EmbeddedError::MissingTenant`] instead of
    /// proceeding.
    pub fn tenant_isolation_enabled(&self) -> bool {
        self.tenant_isolation
    }

    /// PART 6 — resolve the effective tenant id for a single
    /// operation. Per-call `per_call` wins; otherwise the
    /// builder's `default_tenant_id` is used. When isolation
    /// is enabled AND nothing resolves, returns
    /// `Err(MissingTenant)` so the caller surfaces the same
    /// shape as the bridge's HTTP 401 path.
    pub(crate) fn resolve_tenant(
        &self,
        per_call: Option<&str>,
        op: &'static str,
    ) -> Result<Option<String>, EmbeddedError> {
        let effective = per_call
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string())
            .or_else(|| self.default_tenant_id.clone());
        match (&effective, self.tenant_isolation) {
            (None, true) => Err(EmbeddedError::MissingTenant { op }),
            _ => Ok(effective),
        }
    }
}

/// Builder for [`RelixEmbedded`]. Defaults:
///
/// - Memory: in-memory SQLite (no path → `:memory:`).
/// - Provider: required. The builder returns an error if no provider
///   is configured at [`build`](Self::build) time, because there is no
///   sensible default — a silent fallback to a mock would mask bugs
///   in production callers.
/// - `default_model`: empty (provider picks).
/// - `chunk_size_chars`: 800 (matches the runtime's
///   `DEFAULT_CHUNK_SIZE_CHARS`).
#[derive(Default)]
pub struct RelixEmbeddedBuilder {
    memory_path: Option<PathBuf>,
    provider: Option<Arc<dyn ChatProvider>>,
    default_model: String,
    chunk_size_chars: Option<usize>,
    /// PART 6: see [`RelixEmbedded::default_tenant_id`].
    default_tenant_id: Option<String>,
    /// PART 6: see [`RelixEmbedded::tenant_isolation_enabled`].
    tenant_isolation: bool,
}

impl RelixEmbeddedBuilder {
    /// Path to the SQLite memory database. When unset, the builder
    /// opens an in-memory database (useful for tests; data is lost
    /// when the runtime drops). Parent directories are created
    /// automatically.
    pub fn memory_db(mut self, path: impl Into<PathBuf>) -> Self {
        self.memory_path = Some(path.into());
        self
    }

    /// Set the AI provider. Required — call this before
    /// [`build`](Self::build) or the build call will error.
    ///
    /// Any `ChatProvider` implementation works:
    ///
    /// - [`relix_runtime::nodes::ai::provider::MockProvider`] for tests
    /// - [`relix_runtime::nodes::ai::provider::openai_compat::OpenAICompatibleProvider`]
    ///   for Ollama / OpenAI / OpenRouter / xAI
    /// - The Anthropic / Gemini providers in the same module
    /// - Any custom impl behind an `Arc<dyn ChatProvider>`
    pub fn provider(mut self, provider: Arc<dyn ChatProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    /// Default model id sent on chat requests that don't override.
    /// Empty string means "let the provider pick its built-in default".
    pub fn default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    /// Override the document-ingest chunk size. Defaults to 800
    /// characters; minimum clamped to 64.
    pub fn chunk_size_chars(mut self, chunk_size_chars: usize) -> Self {
        self.chunk_size_chars = Some(chunk_size_chars);
        self
    }

    /// PART 6: set the default tenant id every operation falls
    /// back to when its per-call `tenant_id` field is `None`
    /// or empty. Leave unset for "no default" — when
    /// [`Self::tenant_isolation`] is `true` AND there's no
    /// default AND no per-call tenant, the operation returns
    /// [`EmbeddedError::MissingTenant`].
    pub fn default_tenant_id(mut self, id: impl Into<String>) -> Self {
        let s = id.into();
        self.default_tenant_id = if s.trim().is_empty() { None } else { Some(s) };
        self
    }

    /// PART 6: enable fail-closed tenant isolation. When
    /// `true`, every operation that resolves to no tenant id
    /// (per-call empty AND `default_tenant_id` unset) returns
    /// [`EmbeddedError::MissingTenant`] rather than
    /// silently filing into the no-tenant bucket. Also
    /// propagates to the underlying [`LayeredMemoryStore`]
    /// via `open_with_tenant_isolation`, so SQLite reads
    /// apply `WHERE tenant_id = ?` filtering and writes
    /// stamp the per-record `tenant_id` column.
    pub fn tenant_isolation(mut self, enabled: bool) -> Self {
        self.tenant_isolation = enabled;
        self
    }

    /// Boot the runtime. Async because the memory store opens its
    /// SQLite file inside a `spawn_blocking` so the caller's
    /// runtime doesn't take the open hit.
    pub async fn build(self) -> Result<RelixEmbedded, EmbeddedError> {
        let Some(provider) = self.provider else {
            return Err(EmbeddedError::Config(
                "no provider configured; call .provider(Arc::new(...)) before .build()".into(),
            ));
        };
        let memory_path = self.memory_path;
        let tenant_isolation = self.tenant_isolation;
        // PART 6: thread `tenant_isolation` through the store
        // open call so the SQLite fallback path applies the
        // `WHERE tenant_id = ?` filter on every read.
        let memory = tokio::task::spawn_blocking(move || match memory_path {
            Some(path) => LayeredMemoryStore::open_with_tenant_isolation(&path, tenant_isolation),
            None => LayeredMemoryStore::in_memory_with_tenant_isolation(tenant_isolation),
        })
        .await
        .map_err(|e| EmbeddedError::Config(format!("memory bootstrap task: {e}")))??;
        let chunk_size_chars = self.chunk_size_chars.unwrap_or(800).max(64);
        Ok(RelixEmbedded {
            memory,
            provider,
            default_model: self.default_model,
            chunk_size_chars,
            history: Arc::new(HistoryStore::default()),
            default_tenant_id: self.default_tenant_id,
            tenant_isolation,
        })
    }
}
