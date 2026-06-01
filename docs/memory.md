# Memory

Memory in Relix has five layers served by the same memory node.
Two SQLite databases back the system: the **Hermes store**
(`db_path`) holds chat turns, FTS5 indexes, per-subject agent
text blobs, and embedding chunks; the **four-layer store**
(`layered_db_path`) holds structured `memory_records` rows
across Raw / Semantic / Observation / Model tiers with
bi-temporal validity, quarantine, and Qdrant mirroring.

This doc is the index — it names every capability, surface, and
route in the 0.4.1 release. Detailed designs live in the linked
docs.

## Overview of layers

| # | Name | Store | Capability surface |
|---|------|-------|--------------------|
| 1 | Chat-turn store | `turns` + `turns_fts` (Hermes SQLite) | `memory.write_turn`, `memory.recent_for_session`, `memory.search_turns` |
| 2 | Embedding store | `memory_embeddings` (Hermes SQLite) | `memory.embed`, `memory.search`, `memory.embed_all` |
| 3 | Agent text blobs | `agent_memory` (Hermes SQLite) | `memory.agent_read`, `memory.agent_write`, `memory.agent_curate` |
| 4a–d | Four-layer record store | `memory_records` (layered SQLite + Qdrant) | `memory.records_search`, `memory.dialectic`, `memory.ingest_*`, `memory.context_flush`, quarantine, inspect |
| — | Cross-cutting | Both databases | `memory.session_search`, `memory.pii_scan`, `memory.anonymize_preview`, `memory.bulk_anonymize`, `memory.curator_status` |

Layers 1–3 are the **Hermes-style** surface and are always
active. Layer 4 activates only when `[memory.qdrant]` is
configured; without it, `memory.records_search` and all
GAP-5+ capabilities are unavailable.

The three search capabilities are **distinct** and must not be
confused:

| Capability | Index | Scope |
|---|---|---|
| `memory.search_turns` | FTS5 keyword index on `turns` | All sessions, full-text substring |
| `memory.search` | Cosine scan on `memory_embeddings` | Per `(subject_id, target)` pair |
| `memory.records_search` | Qdrant cosine + SQLite LIKE fallback | Four-layer `memory_records`, all sources |

## Layer 1 — Chat-turn store

Every chat call writes one row per message. The FTS5 virtual
table `turns_fts` mirrors the `body` column so full-text search
is cheap across all sessions.

| Capability | Arg | Returns |
|---|---|---|
| `memory.write_turn` | `session_id\|role\|body` | `ok\n` |
| `memory.recent_for_session` | `session_id` or `session_id\|N` | `role: text\n` per turn, oldest first; default N=10 |
| `memory.search_turns` | `query` or `query\|N` | `session_id\trole\ttext\n` per FTS5 match; default N=10 |

`N` is clamped to `[1, max_n]` where `max_n` defaults to 100.
The `turns` table also carries a `flushed` column (added by
`memory.context_flush`) that marks turns already promoted to the
four-layer Semantic tier.

### RAG (Retrieval-Augmented Generation)

When `[ai.memory_peer] rag_enabled = true`, every `ai.chat`
call:

1. Embeds the user's prompt locally (in-process, no extra hop).
2. Calls `memory.search` on the memory peer with the precomputed
   embedding as `|embedding=<base64 LE f32>` — the memory node
   skips its own outbound embed call when this field is present.
3. Merges hits from both `agent` and `user` targets, drops any
   below `rag_min_score`, takes the top `rag_top_k`, and formats
   the block as:

   ```
   --- Relevant context from memory ---
   [score: 0.92] (agent) <chunk text>
   [score: 0.87] (user)  <other chunk>
   ---
   ```

4. Injects the block into `system_prompt` after the agent/user
   memory block.

Configuration in `[ai.memory_peer]`:

| Key | Default | Meaning |
|---|---|---|
| `rag_enabled` | `false` | Enable cross-session RAG retrieval |
| `rag_top_k` | `5` | Max hits in the formatted block |
| `rag_min_score` | `0.70` | AI-node cosine floor (distinct from Qdrant's `score_threshold = 0.75`) |

If no hits clear `rag_min_score`, or if the memory peer is
unreachable, the RAG block is omitted silently. `ai.chat` never
fails because RAG failed.

### Automatic history injection

When `[ai.memory_peer]` is configured, the AI node calls
`memory.recent_for_session` automatically before every `ai.chat`
— flows no longer need a manual `remote_call` for history. The
fetched `role: text\n` block merges with the caller-supplied
`history` field; auto-fetched lines appear first (older context).
Configure the per-call cap with `[ai.memory_peer]
max_history_turns` (default 10).

## Layer 2 — Embedding store

Each text chunk can be embedded through an AI peer (`ai.embed`)
and stored with a `(subject_id, target)` key. Cosine top-K
surfaces topically related chunks.

| Capability | Arg | Returns |
|---|---|---|
| `memory.embed` | `subject_id\|target\|text` | `embedding_id=<id>\n` (new) or `ok\|embedding_id=<id>\n` (dedup) |
| `memory.search` | `subject_id\|target\|query[\|limit][\|embedding=<b64>]` | `embedding_id\tscore\tchunk\n` per hit, then `count=N\n` |
| `memory.embed_all` | `subject_id` | `ok\|chunks_embedded=N\n` |

`target` is `"agent"` or `"user"`. Default `limit` is 5, max 20.
Dedup uses `blake3(chunk_text)` per `(subject_id, target, entry_hash)`;
re-embedding the same text returns the existing row.

Requires `[memory.embedding_peer]` in config. Without it the
three capabilities register but return a clear
`embedding dispatcher not configured` error.

Full design: [`vector-memory.md`](vector-memory.md).

## Layer 3 — Agent text blobs

Two text stores per agent, keyed by `subject_id` and capped at
2200 (`agent`) / 1375 (`user`) characters. Entries are separated
by `§` (U+00A7).

| Capability | Arg | Returns |
|---|---|---|
| `memory.agent_read` | `subject_id` | `agent_bytes=N\|user_bytes=M\n<N bytes><M bytes>` |
| `memory.agent_write` | `subject_id\|target\|action\|data` | `ok\|chars=N\n` (write) or raw content (read) |
| `memory.agent_curate` | `subject_id\|ai_peer_alias` | pipe-delim before/after summary |
| `memory.curator_status` | (none) | pipe-delim `key=value` including `last_run_at`, `next_run_at`, `running`, `agents_reviewed`, `agents_curated`, `total_chars_saved` |

`action` is `add` / `replace` / `remove` / `read`. Writes that
would exceed the cap return `INVALID_ARGS` — the cap is never
silently truncated. Full design: [`agent-memory.md`](agent-memory.md).

## Layer 4 — Four-layer record store

Structured `memory_records` with Raw / Semantic / Observation /
Model tiers, bi-temporal validity, quarantine, sharing, and
Qdrant vector mirroring. Requires `[memory.qdrant]` to be
configured.

Full design: [`four-layer-memory.md`](four-layer-memory.md).

### Search and retrieval

| Capability | Arg | Returns |
|---|---|---|
| `memory.records_search` | `query` or `query\|N` | `id\tlayer\tsource\tscore\ttext\n` per hit + `count=N\n` |
| `memory.dialectic` | JSON `{ observer_id, subject_id, question }` | JSON `{ answer, confidence, sources_used, model_used, fallback_reason? }` |
| `memory.session_search` | `subject_id\|query[\|limit]` | proxied from coordinator `task.session_search`; requires `[memory.curator.coord_peer]` |

`memory.records_search` falls back to SQLite `LIKE` if Qdrant is
unavailable; fallback hits carry `score = 1.0`. The Qdrant score
floor is `0.75` (set by `[memory.embedder] score_threshold`).

### Ingestion

| Capability | Arg | Returns |
|---|---|---|
| `memory.ingest_document` | JSON `IngestDocumentArgs` | JSON `IngestDocumentResponse` |
| `memory.ingest_image` | JSON `IngestImageArgs` | JSON `IngestImageResponse` |
| `memory.context_flush` | JSON `{ session_id, agent_name, keep_recent_n }` | JSON `ContextFlushResponse` |

`memory.ingest_document` and `memory.ingest_image` tag records
`source_trust:external`. Ingested content that scores >= 0.55 on
the anomaly scorer is quarantined automatically.
`memory.context_flush` promotes unflushed turns to the Semantic
tier; `keep_recent_n` defaults to 5.

Document chunk size: 800 chars with 100-char paragraph overlap;
max 5,000 chunks per ingest call. Max image size: 25 MiB.

### Quarantine

| Capability | Arg | Returns |
|---|---|---|
| `memory.quarantine_list` | JSON `{ limit?, source? }` | JSON `ListResponse` |
| `memory.quarantine_approve` | JSON `{ id }` | JSON `ApproveResponse` |
| `memory.quarantine_reject` | JSON `{ id }` | JSON `RejectResponse` |

Quarantine rows never auto-expire. Operators must review and
explicitly approve (re-insert into `memory_records`) or reject
(permanently delete).

### Inspector / editing

| Capability | Arg | Returns |
|---|---|---|
| `memory.edit_record` | JSON `{ id, text }` | JSON `EditResponse` |
| `memory.freeze_record` | JSON `{ id }` | JSON `FreezeResponse` |
| `memory.unfreeze_record` | JSON `{ id }` | JSON `FreezeResponse` |
| `memory.bulk_export` | JSON `{ source, layer? }` | JSON `BulkExportResponse` |
| `memory.request_model_refresh` | JSON `{ source }` | JSON `ModelRefreshResponse` |

Frozen records survive consolidation, context-flush archiving,
and curator compaction. Editing a frozen record clears its
embedding so the background pipeline re-embeds it.

## PII capabilities

Always registered regardless of four-layer config:

| Capability | Arg | Returns |
|---|---|---|
| `memory.pii_scan` | JSON `{ "text": "..." }` | JSON `{ "spans": [...], "count": N }` |
| `memory.anonymize_preview` | JSON `{ "text": "...", "strategy": "redact\|pseudonymize\|allow" }` | JSON `{ "anonymized": "...", "spans": [...] }` |
| `memory.bulk_anonymize` | (none) | JSON with per-table scanned/changed counts |

`memory.pii_scan` scans arbitrary text regardless of whether
`[memory.pii] enabled = true`. `memory.bulk_anonymize` requires
`enabled = true` and is idempotent (repeat runs produce zero
changes). See [`memory-security.md`](memory-security.md) for the
full PII and poisoning guard documentation.

## Bridge HTTP surface

| Method | Path | Capability |
|---|---|---|
| GET | `/v1/memory/agent` | `memory.agent_read` (caller-subject-only; 403 on mismatch) |
| POST | `/v1/memory/curate` | `memory.agent_curate` |
| GET | `/v1/memory/curator/status` | `memory.curator_status` |
| POST | `/v1/memory/embed` | `memory.embed` (max text 8 KB) |
| POST | `/v1/memory/search` | `memory.search` (max query 2 KB) |
| POST | `/v1/memory/embed_all` | `memory.embed_all` |
| POST | `/v1/memory/dialectic` | `memory.dialectic` (tenant-scoped) |
| POST | `/v1/memory/ingest` | `memory.ingest_document` |
| POST | `/v1/memory/ingest_image` | `memory.ingest_image` |
| POST | `/v1/memory/context_flush` | `memory.context_flush` |
| GET | `/v1/memory/records/:layer` | Direct SQLite read; `?subject_id=&limit=&offset=&text_filter=`; 503 when `[bridge] memory_db_path` unset |
| GET | `/v1/memory/records/:layer/:id` | Get one record by id |
| GET | `/v1/memory/stats` | Aggregate statistics |
| POST | `/v1/memory/pii/scan` | `memory.pii_scan` |
| POST | `/v1/memory/pii/preview` | `memory.anonymize_preview` |
| POST | `/v1/memory/pii/bulk_anonymize` | `memory.bulk_anonymize` |
| GET | `/v1/memory/sessions/search` | `memory.session_search`; `?q=` required; limit 20 default, max 100 |

There is no bridge route for quarantine, edit-record, freeze,
bulk-export, or model-refresh today — those are accessed through
the capability layer directly.

## Policy

Default boot-script policy admits these capabilities for
`chat-users`:

```
mem_recent              memory.recent_for_session
mem_write               memory.write_turn
mem_search_turns        memory.search_turns
mem_search              memory.search
mem_embed               memory.embed
mem_embed_all           memory.embed_all
mem_agent_read          memory.agent_read
mem_agent_write         memory.agent_write
mem_agent_curate        memory.agent_curate
mem_curator_status      memory.curator_status
```

GAP-5+ capabilities (`memory.records_search`, `memory.dialectic`,
`memory.ingest_*`, `memory.context_flush`, quarantine, inspect)
are not in the default policy and must be explicitly granted.

## Dashboard

`#/memory` surfaces:

- **Persistent memory** — read agent + user content with char
  counts vs cap; trigger curation.
- **Curator status** — live `CuratorState`: last/next run times,
  `running` flag, per-run counters (agents reviewed/curated,
  chars saved). This is real data from the `memory.curator_status`
  capability, not a bridge-local estimate.
- **Semantic search** — pick target, type a query, rank by cosine
  score (Hermes embedding store).
- **Embed all entries** — retrofit embeddings; idempotent.

Chat-turn search and the four-layer inspector are not on the
dashboard today.

## CLI

```
relix-cli ops agent-memory --subject-id <hex>

relix-cli ops memory embed \
  --subject-id <hex> --target agent --text "..."

relix-cli ops memory search \
  --subject-id <hex> --target agent --query "..." --limit 5

relix-cli ops memory embed-all --subject-id <hex>
```

All accept `--json` for the raw bridge payload.

## See also

- [`agent-memory.md`](agent-memory.md) — frozen-snapshot design,
  write semantics, curator invariants, and layer-promoter config.
- [`vector-memory.md`](vector-memory.md) — Hermes embedding store,
  Qdrant configuration, background pipeline.
- [`four-layer-memory.md`](four-layer-memory.md) — Raw/Semantic/
  Observation/Model store, bi-temporal validity, promoter, archiver.
- [`memory-security.md`](memory-security.md) — poisoning guard,
  anomaly scoring, PII anonymization, tenant isolation.
- [`chronicle-retention.md`](chronicle-retention.md) — coordinator
  chronicle retention; also covers the memory node's own
  ConsolidationArchiver.
- [`configuration.md`](configuration.md) — `[memory]`,
  `[memory.embedding_peer]`, `[memory.embedder]`,
  `[memory.curator]`, `[memory.qdrant]`, `[memory.pii]` TOML blocks.
