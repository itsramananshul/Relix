# Four-Layer Memory Store

The four-layer store is the production-grade structured memory
infrastructure introduced in GAP 5–8. It sits alongside the
Hermes-style store (turns, embeddings, agent text blobs) and
activates only when `[memory.qdrant]` is configured.

The store backs all of:

- Document and image ingestion
- Semantic consolidation and LLM-derived observations
- Long-lived fact modeling with bi-temporal validity
- Cross-session context flushing
- Qdrant vector mirroring for semantic search
- Quarantine and operator review workflow
- Sharing between agents

## The four layers

All records live in a single `memory_records` table tagged by
`layer`:

| Layer | `layer` value | Populated by | Purpose |
|---|---|---|---|
| 1 Raw | `"raw"` | Every `memory.write_turn` call | Verbatim capture of dialogue turns; source of truth |
| 2 Semantic | `"semantic"` | Promoter (Raw→Semantic), `memory.context_flush`, `memory.ingest_document`, `memory.ingest_image` | Deduplicated, cleaned knowledge units |
| 3 Observation | `"observation"` | Promoter (Semantic→Observation) via LLM extraction | Agent-interpretable factual observations; anomaly-scored at write time |
| 4 Model | `"model"` | Promoter (Observation→Model) | Consolidated world-model per source; rate-limited, invalidates prior model |

All four layers share the same `memory_records` table and the
same bi-temporal validity mechanism.

### Layer 1 — Raw

Every `memory.write_turn` call inserts a `MemoryRecord` with
`layer = "raw"` alongside the `turns` row. The record id is
`blake3(session_id + "|" + role + "|" + body)` first 16 hex
characters — deterministic and idempotent.

Raw records are the origin nodes in the supersession chain. They
accumulate the `consolidated = true` flag once all their
descendant observations are archived.

### Layer 2 — Semantic

Semantic records are deduplicated knowledge units. Near-duplicate
detection uses cosine similarity: any candidate with cosine >=
0.95 against an existing Semantic record for the same `source`
is dropped silently. The dedup threshold constant is
`DEDUP_COSINE_THRESHOLD = 0.95`.

Records promoted from Raw carry the tag `"promoted:semantic"`.
Records written by `memory.context_flush` or ingest paths carry
`source_trust: external` and `source_trust: internal`
respectively.

`memory.context_flush` writes: bulk-promotes unflushed `turns`
rows (where `flushed = 0`) to Semantic records and stamps
`flushed = 1` on the source turns. `keep_recent_n` (default 5)
turns are preserved as unflushed.

### Layer 3 — Observation

The promoter extracts observations from Semantic records by
sending them to the LLM (`ai.chat` via `[memory.curator]
ai_peer`) and parsing dash-prefixed reply lines. Each extracted
line becomes a candidate observation.

Every candidate observation is **anomaly-scored** before
insertion (see [Anomaly scoring](#anomaly-scoring) below).
External-trust observations (from ingested documents) that score
>= 0.55 are quarantined rather than inserted.

Observation records carry the tag `"promoted:observation"` and
`"auto:promoter"`.

### Layer 4 — Model

The promoter aggregates Layer 3 observations per `source` into a
single Layer 4 model record by calling the LLM. Rate-limited: at
most one regeneration per `source` per 3600 seconds
(`MODEL_THROTTLE_SECS = 3600`).

When a new model is generated:

1. The previous model record for the same `source` is
   **superseded**: its `valid_to = now` and
   `superseded_by = new_record.id` are written atomically.
2. The new model record is inserted with `valid_from = now`.

This creates a **supersession chain** — the full history of model
states for a source is always recoverable by following
`superseded_by` forward or walking the chain backward.

## Bi-temporal validity

All four layers share the same bi-temporal validity scheme:

| Column | Type | Meaning |
|---|---|---|
| `valid_from` | `INTEGER` | Unix seconds when this record became valid |
| `valid_to` | `INTEGER?` | Unix seconds when this record was superseded; `NULL` = currently valid |
| `observed_at` | `INTEGER` | Unix seconds when the underlying fact was observed |
| `superseded_by` | `TEXT?` | Id of the successor record; `NULL` on the head |

**Invariants:**

- `valid_from <= observed_at` always (both set to `unix_secs()`
  at record creation).
- `valid_to = NULL` means the record is the current head for
  its `source`.
- `supersede(old_id, new_record, at)` is one atomic transaction:
  stamps `valid_to = at` and `superseded_by = new.id` on the
  old row, then inserts the new row.
- `as_of(at, source, limit)` returns records where
  `valid_from <= at AND (valid_to IS NULL OR valid_to > at)` —
  point-in-time consistent view.
- `supersedes_chain(id)` walks forward max 1024 hops with cycle
  protection.

This scheme enables forensic replay: operators can ask "what did
the memory store believe about source X at time T?" by querying
with `as_of(T, X, N)`.

## The promoter pipeline

`LayerPromoter` runs as a background tokio task, enabled by
`[memory.curator] promotion_enabled = true`.

```toml
[memory.curator]
promotion_enabled       = true
promotion_interval_secs = 300    # tick every 5 minutes
promotion_batch_size    = 20     # records per stage per tick
```

On each tick, three promotion stages run in sequence:

1. **Raw → Semantic** — select up to `batch_size` Layer 1 records
   with no Semantic descendant; embed and dedup-check against
   existing Semantics for the same source; insert non-duplicates.
2. **Semantic → Observation** — select up to `batch_size` Layer 2
   records with no Observation descendant; call LLM to extract
   observations; anomaly-score each candidate; insert accepted
   ones, quarantine scored ones, drop rejected ones.
3. **Observation → Model** — select sources whose observations
   have changed since the last model was generated and that are
   outside the `MODEL_THROTTLE_SECS` window; call LLM to produce
   a consolidated model; supersede the existing model.

`PromotionStats` per tick: `raw_to_semantic`,
`semantic_to_observation`, `observations_to_model`,
`poisoned_skipped`.

## Anomaly scoring

Every Layer 3 Observation candidate is scored by
`score_observation(candidate, existing)` before insertion. The
score is a 0.0–1.0 composite:

| Signal | Score contribution |
|---|---|
| Short text (< 12 chars trimmed) | +0.50 |
| Low specificity (0 domain-specific tokens) | +0.55 |
| Contradicts an existing observation | +0.50 |

Score is clamped at 1.0. Disposition:

| Score | Action |
|---|---|
| >= 0.85 (`REJECT_THRESHOLD`) | Rejected — never stored, logged as warn |
| >= 0.55 (`QUARANTINE_THRESHOLD`) | Quarantined — stored in `memory_quarantine`, requires operator review |
| < 0.55 | Accepted — inserted into `memory_records` |

External-trust content (from `memory.ingest_document` /
`memory.ingest_image`) is quarantined on score >= 0.55 regardless
of origin.

## Quarantine workflow

Quarantined records land in `memory_quarantine`:

```sql
CREATE TABLE memory_quarantine (
    id           TEXT PRIMARY KEY,
    record_json  TEXT NOT NULL,      -- JSON-serialized MemoryRecord
    reason       TEXT NOT NULL,
    queued_at_ms INTEGER NOT NULL,
    source_trust TEXT NOT NULL DEFAULT 'unknown'
);
```

Quarantine rows never auto-expire. Operator workflow:

1. `memory.quarantine_list` — browse pending rows with optional
   `source` filter and `limit`.
2. Review each row's `reason` and `record_json`.
3. `memory.quarantine_approve { id }` — re-inserts the record
   into `memory_records` (applies an anonymizer pass; the record
   is treated as accepted).
4. `memory.quarantine_reject { id }` — permanent deletion from
   the quarantine table.

## Ingestion

### Document ingestion

`memory.ingest_document` accepts `IngestDocumentArgs` JSON with
`source`, `text` (or `base64`), `content_type`
(`markdown` / `txt` / `code` / `pdf`), and optional `tags`.

Chunking: paragraphs split at 800 chars with 100-char overlap.
Max 5,000 chunks per call. Each chunk becomes one Layer 2
Semantic record with `source_trust: external`.

### Image ingestion

`memory.ingest_image` accepts `IngestImageArgs` JSON. Max image
size: 25 MiB (26,214,400 bytes). Image bytes are passed to the
LLM for caption / description extraction, then the description
is chunked and stored as Layer 2 records with
`source_trust: external`.

### Context flush

`memory.context_flush` promotes unflushed session turns to Layer
2 Semantic in bulk:

```json
{ "session_id": "...", "agent_name": "...", "keep_recent_n": 5 }
```

`keep_recent_n` (default 5) preserves the most recent N turns
as unflushed (so the running session still has context). All
older unflushed turns are promoted to Semantic records and
stamped `flushed = 1` on the `turns` table. Flushed records
receive the caller's `tenant_id`.

## Inspector capabilities

Operators can inspect and modify Layer 4 records directly:

| Capability | Effect |
|---|---|
| `memory.edit_record { id, text }` | Replace `text`; clears `embedding` so the pipeline re-embeds; timestamps `last_edited_ms`. Works even on frozen records. |
| `memory.freeze_record { id }` | Sets `frozen = 1`; the record survives curator consolidation, context-flush archiving, and the ConsolidationArchiver. |
| `memory.unfreeze_record { id }` | Clears `frozen = 0`. |
| `memory.bulk_export { source, layer? }` | Exports all matching records as JSON. |
| `memory.request_model_refresh { source }` | Bypasses `MODEL_THROTTLE_SECS`; forces immediate model regeneration for the source. |

## Dialectic synthesis

`memory.dialectic` is an LLM-powered question-answering surface
that synthesises an answer from Layer 4 model + top-K Layer 3
observations:

```json
// Request
{ "observer_id": "...", "subject_id": "...", "question": "..." }

// Response
{
  "answer": "...",
  "confidence": 0.85,
  "sources_used": ["record-id-1", "record-id-2"],
  "model_used": "openrouter/anthropic/claude-3-5-haiku",
  "fallback_reason": null  // present if fell back to observations only
}
```

Default model: `openrouter/anthropic/claude-3-5-haiku`
(overridden by `[memory.curator] dialectic_model`).
`TOP_K_OBSERVATIONS = 5` observations are fetched.

If no Layer 4 model exists, the response is synthesised from
Layer 3 observations only and `fallback_reason` explains why.

## Sharing

`memory_records` carries a sharing sub-schema:

| Column | Meaning |
|---|---|
| `shareable` | Boolean; `true` means the record can be shared |
| `share_policy` | `"none"` / `"explicit"` / `"auto"` |
| `shared_with` | JSON array of agent names |
| `shared_by` | Agent name that initiated sharing |

Sharing is exposed via the knowledge-sharing HTTP surface
(`/v1/knowledge/*`) rather than through the memory capabilities
directly.

## Storage schema

```sql
CREATE TABLE memory_records (
    id             TEXT PRIMARY KEY,
    layer          TEXT NOT NULL,
    text           TEXT NOT NULL,
    source         TEXT NOT NULL DEFAULT '',
    tags           TEXT NOT NULL DEFAULT '[]',
    created_at     INTEGER NOT NULL,
    valid_from     INTEGER NOT NULL,
    valid_to       INTEGER,
    observed_at    INTEGER NOT NULL,
    embedding      BLOB,                      -- LE f32 packed; NULL until embedded
    shareable      INTEGER NOT NULL DEFAULT 0,
    shared_with    TEXT,
    shared_by      TEXT,
    share_policy   TEXT NOT NULL DEFAULT 'none',
    source_trust   TEXT NOT NULL DEFAULT 'internal',
    frozen         INTEGER NOT NULL DEFAULT 0,
    last_edited_ms INTEGER,
    consolidated   INTEGER NOT NULL DEFAULT 0,
    tenant_id      TEXT,
    superseded_by  TEXT
);
```

Indexes:

```sql
CREATE INDEX memory_records_layer_created  ON memory_records(layer, created_at DESC);
CREATE INDEX memory_records_source         ON memory_records(source);
CREATE INDEX memory_records_pending        ON memory_records(observed_at) WHERE embedding IS NULL;
CREATE INDEX memory_records_share_policy   ON memory_records(share_policy) WHERE share_policy != 'none';
CREATE INDEX memory_records_shared_by      ON memory_records(shared_by) WHERE shared_by IS NOT NULL;
CREATE INDEX idx_memory_records_tenant     ON memory_records(tenant_id) WHERE tenant_id IS NOT NULL;
CREATE INDEX memory_records_archive_scan   ON memory_records(layer, observed_at) WHERE frozen = 0 AND valid_to IS NULL;
```

Column additions were gated by `PRAGMA table_info` probes
(idempotent forward-only migration):

- RELIX-7.16: `shareable`, `shared_with`, `shared_by`,
  `share_policy`
- GAP 6/7/8: `source_trust`, `frozen`, `last_edited_ms`,
  `consolidated`
- GAP 23: `tenant_id`
- GAP 18: `superseded_by`

The store lives in a separate SQLite file (`layered_db_path`,
defaulting to `<db_path-stem>.layered.db`), distinct from the
main Hermes database. Both databases use WAL mode,
`foreign_keys = ON`, and `busy_timeout` via
`crate::db::apply_pragmas`.

## Qdrant point payload

Records embedded into Qdrant carry this payload:

```json
{
  "id":         "<memory_record.id>",
  "layer":      "<raw|semantic|observation|model>",
  "text":       "<memory_record.text>",
  "source":     "<memory_record.source>",
  "tags":       ["..."],
  "created_at": 1234567890
}
```

Point integer id: `blake3(record.id)[..8]` decoded as LE u64.
Same record always upserts the same Qdrant point (deterministic).

## Retention (ConsolidationArchiver)

See [`chronicle-retention.md`](chronicle-retention.md) —
Memory node retention section — for the ConsolidationArchiver
6-hour / 30-day cycle and the MemoryIntegrityAuditor 24-hour
audit cycle.

## Tenant isolation

The four-layer store has two independent isolation planes:

**SQLite plane** — opt-in via
`LayeredMemoryStore::open_with_tenant_isolation(path, true)`.
Tenant-aware read methods (`text_search_for_tenant`,
`get_for_tenant`, `list_for_tenant`) filter on the `tenant_id`
column. Calls with a missing or empty `tenant_id` when isolation
is enabled return `LayeredMemoryError::MissingTenant` (fail
closed). Internal maintenance paths (the promoter, archiver,
auditor) use the legacy tenant-blind methods.

**Qdrant plane** — opt-in via `[memory.qdrant]
tenant_isolation = true`. Per-tenant collections named
`{collection_prefix}_{sanitized_tenant_id}`. Calls without a
valid `tenant_id` when isolation is enabled return
`QdrantError::MissingTenant` and are never routed to the shared
collection.

See [`memory-security.md`](memory-security.md) for the full
tenant isolation documentation.

## Background tasks summary

The memory node can run up to four concurrent background tasks:

| Task | Enabled by | Interval |
|---|---|---|
| `spawn_curator_scheduler` | `[memory.curator] enabled = true` | `interval_secs` (default 3600) |
| `EmbeddingPipeline` | `[memory.embedder] enabled = true` | `interval_secs` (default 60) |
| `LayerPromoter` | `[memory.curator] promotion_enabled = true` | `promotion_interval_secs` (default 300) |
| `ConsolidationArchiver` | Always when layered store is active | 6 hours |
| `MemoryIntegrityAuditor` | Always when layered store is active | 24 hours |

All tasks use tokio; none block each other.

## See also

- [`memory.md`](memory.md) — full capability index and bridge
  HTTP surface.
- [`vector-memory.md`](vector-memory.md) — embedding pipeline,
  Qdrant configuration, `memory.records_search` wire format.
- [`memory-security.md`](memory-security.md) — poisoning guard,
  anomaly scoring detail, PII, tenant isolation.
- [`chronicle-retention.md`](chronicle-retention.md) — archiver
  and integrity auditor cycles.
