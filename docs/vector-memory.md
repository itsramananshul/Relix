# Vector Memory

Per-subject embeddings + cosine-similarity search on the memory
node (Hermes embedding store), plus Qdrant-backed vector search
over the four-layer record store. The two surfaces are
**distinct** and serve different purposes:

| Surface | Capability | Backed by | Scope |
|---|---|---|---|
| Hermes embedding store | `memory.search` | `memory_embeddings` SQLite, cosine scan in Rust | Per `(subject_id, target)` pair |
| Four-layer Qdrant search | `memory.records_search` | Qdrant (with SQLite LIKE fallback) | All `memory_records` across layers |

This document covers both surfaces, the background embedding
pipeline, and the full `[memory.qdrant]` and
`[memory.embedder]` configuration.

## Hermes embedding store

### What this adds

Before the embedding store, an agent could:

1. Read all its memory verbatim into the prompt.
2. Run an FTS5 substring search over chat turns
   (`memory.search_turns`).

After the embedding store, it can also:

3. Embed any text chunk (`memory.embed`).
4. Run a semantic search over a subject's embeddings
   (`memory.search`) — "find memories related to *this query*"
   without keyword overlap.
5. Re-embed everything currently stored in a subject's flat-text
   memory (`memory.embed_all`).

### How to enable

Add the embedding peer to the memory controller config:

```toml
[memory.embedding_peer]
addr          = "/ip4/127.0.0.1/tcp/19712"
alias         = "ai"
deadline_secs = 30
model         = "mock-embed"   # or "text-embedding-3-small" with a real provider
dimensions    = 8              # 1536 for OpenAI; 8 for mock
```

The memory controller dials this peer at startup. Without it,
the three capabilities still register but return a clear
`embedding dispatcher not configured` error.

`dimensions` is accepted but not enforced — the store accepts
any vector length and `cosine_similarity` returns 0.0 on length
mismatch, so mixed-model rows rank last rather than crash.

### How to use from a flow

`.sflow`:

```sflow
step embed: memory.embed "subject-abc|agent|rust uses cargo"
step hits:  memory.search "subject-abc|agent|build system|3"
return step.hits.result
```

`.sol`:

```sol
let _id: str  = remote_call("memory", "memory.embed",  "subject-abc|agent|rust uses cargo");
let res: str  = remote_call("memory", "memory.search", "subject-abc|agent|build system|3");
return res;
```

`memory.search` returns tab-separated rows
`embedding_id\tscore\tchunk_text\n` followed by `count=N\n`.
Scores are cosine similarities in `[-1, 1]`; higher is closer.

RAG path: append `|embedding=<base64 LE f32>` to the arg to
pass a precomputed embedding. The memory node skips its own
outbound embed call when this field is present. See
[`memory.md`](memory.md) for the AI node's RAG flow.

### How to use from the dashboard

`#/memory` page → **Embeddings — semantic search** card.

1. Paste a subject_id (the NodeId hex).
2. Pick `agent` or `user` target.
3. Type a query. Hit **Search**.

Results render as a ranked table.

The **Embed all entries** button calls `POST /v1/memory/embed_all`
for the subject — useful to retrofit embeddings onto an existing
memory. It's idempotent: chunks already embedded (matched by
`blake3(text)`) are skipped.

### How to use from HTTP

```
POST /v1/memory/embed
{
  "subject_id": "abcdef…",
  "target":     "agent",
  "text":       "rust uses cargo for builds"
}
→ { "embedding_id": "1234abcd…" }
   or
→ { "embedding_id": "1234abcd…", "already_present": true }

POST /v1/memory/search
{
  "subject_id": "abcdef…",
  "target":     "agent",
  "query":      "build system",
  "limit":      5
}
→ { "results": [ { embedding_id, score, chunk_text }, … ], "count": N }

POST /v1/memory/embed_all
{ "subject_id": "abcdef…" }
→ { "ok": true, "chunks_embedded": N }
```

### Storage shape

```sql
CREATE TABLE memory_embeddings (
  embedding_id TEXT PRIMARY KEY,         -- 16-hex random id
  subject_id   TEXT NOT NULL,
  target       TEXT NOT NULL,            -- "agent" | "user"
  chunk_text   TEXT NOT NULL,
  embedding    BLOB NOT NULL,            -- LE-packed f32
  model        TEXT NOT NULL,
  created_at   INTEGER NOT NULL,
  entry_hash   TEXT NOT NULL,            -- blake3(chunk_text)
  tenant_id    TEXT NOT NULL DEFAULT 'default',  -- added by GROUP 6 tenant isolation
  UNIQUE (subject_id, target, entry_hash)
);
CREATE INDEX memory_embeddings_subject ON memory_embeddings (subject_id, target);
CREATE INDEX memory_embeddings_hash    ON memory_embeddings (entry_hash);
```

`insert()` writes `tenant_id = "default"`;
`insert_for_tenant()` writes the caller's verified tenant.
Searches using tenant-aware APIs filter on `tenant_id`.

Dedup is content-only: same text under the same
`(subject_id, target)` returns the existing row's
`embedding_id`. Re-embedding with a different model produces a
different row because the dedup UNIQUE includes the content hash,
not the model — operators who want to re-embed under a new model
must clear the table explicitly (no API for that yet; planned).

### Performance posture

The first cut uses a **full table scan** filtered by
`(subject_id, target)` with cosine similarity ranked in Rust.
This is intentional:

- The agent + user memory caps are 2200 + 1375 chars per subject.
  Even aggressive operators stay well under a few hundred rows
  per subject.
- A linear scan over a few hundred f32-dot-products is on the
  order of microseconds.
- Avoids pulling in `sqlite-vec` or an HNSW index dep.

Upgrade path when this hurts:

- Replace the scan with `sqlite-vec`'s `vec0` virtual table.
- Or add an in-memory HNSW cache keyed by `(subject_id, target)`.

Both options are local to `nodes/memory/embeddings.rs` —
callers go through `EmbeddingStore::search` so nothing else
changes.

### What model to use

- **`mock-embed`** — built into `MockProvider`. Deterministic
  8-dim vectors from `blake3(text)`. Good enough for local demos
  and CI; not semantically meaningful.
- **`text-embedding-3-small`** (OpenAI) — 1536 dims. Set
  `RELIX_OPENAI_API_KEY` and switch `embedding_peer.model`.
- **Any OpenAI-compatible local server** (Ollama, LM Studio,
  vLLM) — same wire shape as OpenAI.

Anthropic and Gemini providers have no embedding API in their
bindings today; they return `Permanent("not supported")` and
the operator gets a clear error.

## Qdrant-backed four-layer search

`memory.records_search` is a separate, higher-level search
capability over the `memory_records` four-layer store. It
requires `[memory.qdrant]` to be configured.

### Wire format

```
memory.records_search
  arg: query        (default N=10)
   or: query|N
returns: id\tlayer\tsource\tscore\ttext\n per hit
         count=N\n
```

Flow:

1. Embed the query via `EmbeddingDispatcher`.
2. Resolve the tenant's Qdrant collection (fails closed with
   `MissingTenant` when `tenant_isolation = true` and no
   `tenant_id` is present).
3. Query Qdrant with cosine distance, applying `score_threshold`
   (default `0.75`) as a floor.
4. On Qdrant error or dispatcher unavailability, fall back to
   SQLite `LIKE` search; fallback hits carry `score = 1.0`.

### Configuration

```toml
[memory.qdrant]
url               = "http://localhost:6333"  # empty = disabled
collection        = "relix_memory"           # fallback when tenant_isolation = false
dim               = 1536                     # vector dimensionality (alias: embedding_dim)
api_key           = "..."                    # SecretString, zeroized on drop
tenant_isolation  = false                    # true = per-tenant collections
collection_prefix = "relix"                  # prefix for per-tenant collection names
```

Auth is `api-key: <key>` HTTP header — **not** `Authorization:
Bearer`. Reqwest client timeout: 10 seconds. Distance metric:
always Cosine.

**Tenant isolation** (`tenant_isolation = true`): collection
names are derived as `{collection_prefix}_{sanitized_tenant_id}`.
`sanitize_tenant_id` maps non-alphanumeric / non-underscore
characters to `_`, trims leading/trailing `_`, defaults empty
input to `"default"`, and truncates to 63 characters (Qdrant's
collection name limit). Calls with a missing or empty
`tenant_id` when `tenant_isolation = true` fail closed with
`QdrantError::MissingTenant` — they are never routed to the
shared fallback collection.

Example collection names with `collection_prefix = "relix"`:

| `tenant_id` | Derived collection |
|---|---|
| `acme` | `relix_acme` |
| `acme-corp` | `relix_acme_corp` |
| `acme/tenant.1` | `relix_acme_tenant_1` |
| (empty) | `relix_default` |

### Background embedding pipeline

The background pipeline continuously embeds new `memory_records`
rows and upserts their vectors into Qdrant. Requires
`[memory.embedder] enabled = true`.

```toml
[memory.embedder]
enabled        = false   # must opt in; false by default
batch_size     = 32      # records per embed call
interval_secs  = 60      # tick interval
score_threshold = 0.75   # cosine floor applied to Qdrant search results
```

Note: `score_threshold` here is the Qdrant query-time filter
applied by `memory.records_search`. It is distinct from the AI
node's `rag_min_score = 0.70`, which filters Hermes-store hits
before prompt injection.

Pipeline behavior per tick:

1. Fetch all `memory_records` rows where `embedding IS NULL`.
2. Batch into groups of `batch_size`, call `EmbeddingDispatcher::embed`.
3. Run a PII anonymization pass (defense-in-depth) on text
   before the embed call and before writing to the Qdrant payload.
4. Group upserts by tenant bucket; records with a missing
   `tenant_id` when `tenant_isolation = true` are skipped and
   logged as WARN — never routed to a shared collection.
5. Write the embedding blob back to the `memory_records.embedding`
   column.

## Wire shape summary

| Capability | Arg | Return |
|---|---|---|
| `memory.embed` | `subject_id\|target\|text` | `embedding_id=<id>\n` (new) or `ok\|embedding_id=<id>\n` (dedup) |
| `memory.search` | `subject_id\|target\|query[\|limit][\|embedding=<b64>]` | `embedding_id\tscore\tchunk\n` per hit, then `count=N\n` |
| `memory.embed_all` | `subject_id` | `ok\|chunks_embedded=N\n` |
| `memory.records_search` | `query` or `query\|N` | `id\tlayer\tsource\tscore\ttext\n` per hit, then `count=N\n` |
| `ai.embed` | `model\|text1§text2§…` | `model\|base64(f32_le_1)\|base64(f32_le_2)\|…\n` |

Embedding blob encoding: LE-packed `f32`. 1536 dims = 6144 bytes.

Point id minting for Qdrant: `blake3(record.id)[..8]` decoded as
LE u64 — deterministic so the same record always upserts the
same Qdrant point.
