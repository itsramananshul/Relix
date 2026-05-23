# Vector memory

Per-subject embeddings + cosine-similarity search on the memory
node. Lives alongside the existing flat-text agent memory: the
two surfaces don't replace each other — flat text is still the
canonical record an agent reads in its system prompt, and
embeddings are an *additional* retrieval surface flows / tools
can use to find topically related chunks without a full read.

## What this adds

Before this lands, an agent could:

1. Read all its memory verbatim into the prompt and let the LLM
   pick what's relevant.
2. Run an FTS5 substring search over chat turns
   (`memory.search_turns`).

After this lands, it can also:

3. Embed any memory entry (`memory.embed`).
4. Run a semantic search over a subject's embeddings
   (`memory.search`) — "find memories related to *this query*"
   without keyword overlap.
5. Re-embed everything currently stored in a subject's flat-text
   memory (`memory.embed_all`).

## How to enable

Add the embedding peer to the memory controller config. The
mesh boot script (`scripts/relix-mesh-up.ps1`) writes this
block by default — pointing at the AI peer's `ai.embed`
capability:

```toml
[memory.embedding_peer]
addr          = "/ip4/127.0.0.1/tcp/19712"
alias         = "ai"
deadline_secs = 30
model         = "mock-embed"   # or "text-embedding-3-small" with a real provider
dimensions    = 8              # 1536 for OpenAI; 8 for the mock provider
```

The memory controller dials this peer at startup and populates
the embedding-dispatcher cell. With it unset, the three new
capabilities still register but return a clear
`memory.embed: embedding dispatcher not configured (missing
[memory.embedding_peer])` error rather than `unknown method`.

`dimensions` is reserved for a future schema check — today the
store accepts any vector length and `cosine_similarity` returns
0.0 on length mismatch, so mixed-model rows rank last rather
than crash.

## How to use from a flow

`.sflow` example:

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

## How to use from the dashboard

`#/memory` page → **Embeddings — semantic search** card.

1. Paste a subject_id (the NodeId hex — same one the existing
   memory inspector uses).
2. Pick `agent` or `user` target.
3. Type a query. Hit **Search**.

Results render as a ranked table.

The **Embed all entries** button calls `POST /v1/memory/embed_all`
for the subject — useful when you want to retrofit embeddings
onto an existing memory that was authored before this feature
landed. It's idempotent: chunks already embedded (matched by
`blake3(text)`) are skipped, and the returned `chunks_embedded`
is the total count of covered chunks (not the delta).

## How to use from the CLI

```sh
relix-cli ops memory embed \
  --subject-id <NodeId-hex> \
  --target agent \
  --text "rust uses cargo for builds"

relix-cli ops memory search \
  --subject-id <NodeId-hex> \
  --target agent \
  --query "build system" \
  --limit 5

relix-cli ops memory embed-all --subject-id <NodeId-hex>
```

All three accept `--json` for the raw bridge payload.

## How to use from HTTP

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

## What model to use

- **`mock-embed`** — built into the AI node's `MockProvider`.
  Deterministic 8-dim vectors derived from `blake3(text)`. Same
  text always returns the same vector. Identical inputs cosine
  to 1.0; different inputs are reliably distinct. This is what
  the mesh boot script wires by default so the end-to-end embed
  / search pipeline works with no real OpenAI key. Good enough
  for local demos and CI; **not** semantically meaningful — the
  vectors carry no actual topic structure.
- **`text-embedding-3-small`** (OpenAI) — 1536 dims. The
  AI node's `OpenAICompatibleProvider` calls
  `POST {base_url}/embeddings` with this as the default model.
  Set `RELIX_OPENAI_API_KEY` and switch the memory
  `embedding_peer.model` field.
- **Any OpenAI-compatible local server** (Ollama, LM Studio,
  vLLM with the embeddings endpoint) — same wire shape as
  OpenAI. Set `[ai.providers.local]` with `base_url`
  and `default_model` to the local embedding model name.

The other providers (Anthropic, Gemini) have no embedding API
in their bindings today; they return
`Permanent("not supported")` from `generate_embeddings` and the
operator gets a clear error rather than a silent failure.

## Performance posture

The first cut uses a **full table scan** filtered by
`(subject_id, target)` with cosine similarity ranked in Rust.
This is intentional:

- The agent + user memory caps are 2200 + 1375 chars per
  subject. Even an aggressive operator authoring small chunks
  stays well under a few hundred rows per subject.
- A linear scan over a few hundred f32-dot-products is on the
  order of microseconds.
- Avoids pulling in the `sqlite-vec` extension or an HNSW
  index dep, both of which carry build complexity.

Upgrade path when this hurts:

- Replace the scan with `sqlite-vec`'s `vec0` virtual table —
  the Rust binding ships a `LOAD EXTENSION` shim; local change
  to `EmbeddingStore::search`.
- Or add an in-memory HNSW cache keyed by `(subject_id,
  target)` that rebuilds on controller startup.

Both options are local to `nodes/memory/embeddings.rs` —
callers go through `EmbeddingStore::search` so nothing else
changes.

## Storage shape

SQLite table on the memory node:

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
  UNIQUE (subject_id, target, entry_hash)
);
CREATE INDEX memory_embeddings_subject ON memory_embeddings (subject_id, target);
CREATE INDEX memory_embeddings_hash    ON memory_embeddings (entry_hash);
```

Dedup is content-only: same text under the same
`(subject_id, target)` returns the existing row's
`embedding_id`. Re-embedding with a different model produces a
*different* row because the primary key is the random
`embedding_id`, but the dedup UNIQUE still kicks in — operators
who want to re-embed under a new model must clear the table
explicitly (no API for that yet; planned).

## Wire shape (mesh capabilities)

| Method | Arg | Return |
|---|---|---|
| `memory.embed` | `subject_id\|target\|text` | `embedding_id=<id>\n` (new) or `ok\|embedding_id=<id>\n` (dedup) |
| `memory.search` | `subject_id\|target\|query[\|limit]` (default 5, max 20) | `<id>\t<score>\t<chunk>\n` per hit, then `count=N\n` |
| `memory.embed_all` | `subject_id` | `ok\|chunks_embedded=N\n` |
| `ai.embed` | `model\|text1§text2§…` | `model\|base64(f32_le_1)\|base64(f32_le_2)\|…\n` |

The base64 layer in `ai.embed` keeps the response ASCII without
inventing a CBOR envelope just for this one path. The memory
node's `EmbeddingMeshDispatcher` decodes it back to `Vec<f32>`
before calling cosine.
