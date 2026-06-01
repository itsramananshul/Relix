# Embedding Relix In-Process (`relix-embedded`)

> Version 0.4.1

`relix-embedded` lets a Rust application run the Relix chat + four-layer
memory stack in-process with zero external binaries. It wraps
`relix_runtime::nodes::ai::provider::ChatProvider` (the same AI provider
trait used by the standalone AI node) and
`relix_runtime::nodes::memory::schema::LayeredMemoryStore` (the same
SQLite-backed store used by the standalone memory node), and exposes them
through a small `async` API built with the builder pattern.

## What's included

| Capability | Notes |
|---|---|
| AI provider chat | Any `ChatProvider` impl (OpenAI-compatible, Anthropic, Gemini, Mock, or custom) plugs in directly. No HTTP, no mesh. |
| Four-layer memory store | Chunked document ingest, text search (`LIKE`-based on SQLite FTS), and direct record CRUD. Layers: `raw`, `semantic`, `observation`, `model`. |
| In-process conversation history | LRU-bounded ring of the last 20 turns per session (up to 10 000 concurrent sessions). |
| Tenant isolation | Optional hard isolation: when enabled, every memory read/write and chat is scoped to a verified `tenant_id`. |

## What's NOT included

- **libp2p mesh networking.** No peer discovery, no remote-peer dispatch.
- **The web bridge HTTP server.** Run `relix-web-bridge` if you need HTTP.
- **The CLI.** Nothing here reads `~/.relix/` configs.
- **Multi-node federation.** No coordinator persistence, no cross-node flows.

The point of embedded mode is that the caller's app stays the only running
process. When the app outgrows single-process constraints it should switch
to the full mesh.

## Quick start

Add the dependency:

```toml
[dependencies]
relix-embedded = "0.4.1"
relix-runtime  = "0.4.1"   # for the ChatProvider impls
```

Build the runtime handle, ingest a document, chat:

```rust
use std::sync::Arc;

use relix_embedded::{ChatInput, MemoryIngestInput, MemorySearchInput, RelixEmbedded};
use relix_runtime::nodes::ai::provider::MockProvider;

#[tokio::main]
async fn main() -> Result<(), relix_embedded::EmbeddedError> {
    let relix = RelixEmbedded::builder()
        .provider(Arc::new(MockProvider))   // REQUIRED — no default
        .memory_db("./relix-memory.db")     // omit for ephemeral :memory:
        .default_model("gpt-4o-mini")       // optional; provider picks when empty
        .chunk_size_chars(800)              // optional; default 800, min 64
        .tenant_isolation(false)            // optional; default false
        .build()
        .await?;

    // Ingest a document.
    relix.memory_ingest_document(MemoryIngestInput {
        subject_id: "user-123".into(),
        content: "Pricing tier B is $99/mo".into(),
        content_type: "markdown".into(),
        source: "notes.md".into(),
        tenant_id: None,
    }).await?;

    // Chat — provider sees the last 20 turns for the session automatically.
    let resp = relix.chat(ChatInput {
        session_id: "user-123".into(),
        message: "What's the price of tier B?".into(),
        agent_name: "assistant".into(),
        model: None,
        system_prompt: None,
        tenant_id: None,
    }).await?;
    println!("{}", resp.text);

    // Search memory.
    let hits = relix.memory_search(MemorySearchInput {
        query: "pricing".into(),
        subject_id: "user-123".into(),
        limit: 5,
        tenant_id: None,
    }).await?;
    for hit in hits {
        println!("[{}] {}", hit.layer, hit.text);
    }

    Ok(())
}
```

## Provider options

`provider(Arc<dyn ChatProvider>)` is the only required builder call.
Compatible implementations live in `relix_runtime::nodes::ai::provider`:

| Implementation | Notes |
|---|---|
| `MockProvider` | Returns deterministic stub replies. Useful for tests. |
| `openai_compat::OpenAICompatibleProvider` | Works with any OpenAI-compatible endpoint (OpenAI, Ollama, Azure OpenAI, etc.). |
| Anthropic provider | Native Anthropic Messages API. |
| Gemini provider | Google Gemini. |
| Custom `Arc<dyn ChatProvider>` | Implement the `ChatProvider` trait from `relix-runtime` directly. |

## Builder reference

```rust
RelixEmbedded::builder()
    .provider(Arc<dyn ChatProvider>)     // REQUIRED
    .memory_db(path)                     // default: in-memory SQLite (lost on drop)
    .default_model(model)               // default: "" (provider picks)
    .chunk_size_chars(n)                // default: 800; clamped to min(n, 64)
    .tenant_isolation(bool)             // default: false
    .default_tenant_id(id)              // default: None; whitespace-only treated as None
    .build().await                      // -> Result<RelixEmbedded, EmbeddedError>
```

`RelixEmbedded` is `Clone` — cloning shares the connection pool and
provider `Arc` cheaply. Safe to distribute across `tokio` tasks.

## API surface

### `chat`

```rust
pub async fn chat(&self, input: ChatInput) -> Result<ChatResponse, EmbeddedError>
```

`ChatInput` fields: `session_id` (required, non-empty), `message` (required,
non-empty), `agent_name`, `model` (overrides `default_model` when set),
`system_prompt`, `tenant_id`.

`ChatResponse` fields: `text`, `provider`, `model`, `usage: Option<UsageReport>`.

`UsageReport` fields: `prompt_tokens: u32`, `completion_tokens: u32`,
`total_tokens: u32`.

**Flow:** validates inputs → resolves tenant → loads history ring (last
20 turns) → calls provider → persists both turns to memory as
`MemoryLayer::Raw` records (best-effort; a memory write failure is logged
as WARN but never fails the chat response) → updates history ring.

### `memory_ingest_document`

```rust
pub async fn memory_ingest_document(
    &self,
    input: MemoryIngestInput,
) -> Result<MemoryIngestResult, EmbeddedError>
```

`MemoryIngestInput` fields: `subject_id`, `content`, `content_type`,
`source`, `tenant_id`.

Valid `content_type` values: `"markdown"`, `"md"`, `"txt"`, `"code"`,
`"text"`.

Chunks by double-newline paragraph with 100-character overlap. Each chunk
stored as a `MemoryLayer::Semantic` record tagged with
`source:<source>` and `content_type:<content_type>`. Rejects inputs
that produce zero chunks or more than 5 000 chunks.

`MemoryIngestResult` fields: `chunks_created: usize`, `subject_id`,
`source`, `content_type`.

### `memory_search`

```rust
pub async fn memory_search(
    &self,
    input: MemorySearchInput,
) -> Result<Vec<MemoryHit>, EmbeddedError>
```

`MemorySearchInput` fields: `query`, `subject_id`, `limit`, `tenant_id`.
`limit = 0` is coerced to `5`.

`MemoryHit` fields: `id: String`, `text: String`, `source: String`,
`layer: String` (e.g. `"raw"`, `"semantic"`), `tags: Vec<String>`,
`observed_at: i64` (unix seconds).

When `tenant_isolation = true` the search is scoped to the resolved
tenant via `LayeredMemoryStore::text_search_for_tenant`. When
`tenant_isolation = false` it calls `text_search` and post-filters by
`subject_id`.

### Accessors

```rust
pub fn memory_store(&self) -> &LayeredMemoryStore
pub fn provider(&self) -> &Arc<dyn ChatProvider>
pub fn default_model(&self) -> &str
pub fn default_tenant_id(&self) -> Option<&str>
pub fn tenant_isolation_enabled(&self) -> bool
pub fn session_turn_count(&self, session_id: &str) -> usize
```

## Tenant isolation

When `tenant_isolation(true)` is set on the builder:

- Every `chat`, `memory_ingest_document`, and `memory_search` call must
  supply a `tenant_id` — either per-call in the `Input` struct or as
  `default_tenant_id` on the builder. Whitespace-only strings are treated
  as absent.
- A call with no resolvable tenant returns
  `EmbeddedError::MissingTenant { op: "chat" }` (or the corresponding op
  name) before any provider call.
- The `LayeredMemoryStore` is opened with tenant isolation enabled so that
  storage-layer reads are filtered to the tenant row.

When `tenant_isolation(false)` (the default), `tenant_id` fields on all
`Input` structs are optional and ignored for isolation purposes.

**Session history is keyed by `session_id` only** — there is no
per-tenant namespace in the in-process history ring. If you need strict
cross-tenant session isolation, namespace the `session_id` yourself (e.g.
`"acme::session-123"`).

## Error handling

```rust
use relix_embedded::EmbeddedError;

match relix.chat(input).await {
    Ok(resp) => println!("{}", resp.text),
    Err(EmbeddedError::Config(msg))          => eprintln!("bad config: {msg}"),
    Err(EmbeddedError::Memory(e))            => eprintln!("memory layer: {e}"),
    Err(EmbeddedError::Provider(msg))        => eprintln!("provider: {msg}"),
    Err(EmbeddedError::Ingest(msg))          => eprintln!("ingest: {msg}"),
    Err(EmbeddedError::MissingTenant { op }) => eprintln!("tenant required for {op}"),
}
```

Memory write failures during chat are best-effort — they are WARN-logged
and never surfaced as `EmbeddedError`.

## Memory record IDs

Record IDs are deterministic BLAKE3 hashes:

| Layer | ID prefix | Hash input |
|---|---|---|
| Raw (chat turn) | `raw-` | `session_id \| "\|" \| body \| "\|" \| seq_u64_le` |
| Semantic (ingest chunk) | `sem-` | `subject_id \| "\|" \| source \| "\|" \| idx_u64_le \| "\|" \| body` |

Each prefix is followed by the first 16 hex characters of the BLAKE3
digest.

## See also

- [`docs/four-layer-memory.md`](four-layer-memory.md) — the memory layer
  model (`raw`, `semantic`, `observation`, `model`) that `relix-embedded`
  uses directly.
- [`docs/provider-configuration.md`](provider-configuration.md) — how to
  configure the `OpenAICompatibleProvider` and other providers that plug
  into `ChatProvider`.
- [`crates/relix-embedded/src/lib.rs`](../crates/relix-embedded/src/lib.rs)
  — the crate's `//!` header, which has a self-contained `no_run` usage
  example.
