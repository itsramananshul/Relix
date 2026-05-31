//! Four-layer memory schema + the SQLite-backed
//! [`LayeredMemoryStore`] that persists [`MemoryRecord`] rows.
//!
//! This module is the durable home of memory records as they
//! flow through the layered model:
//!
//! 1. **Raw** — verbatim turn content captured on every
//!    `memory.write_turn`.
//! 2. **Semantic** — chunked, deduplicated rewrites the
//!    embedding pipeline produces from raw records.
//! 3. **Observation** — extracted facts (`"user prefers
//!    Helvetica"`); produced by the agent's curator.
//! 4. **Model** — living model of the agent / user; the
//!    highest-level summary surface that backs persistent
//!    `MEMORY.md` / `USER.md`.
//!
//! Records carry bi-temporal validity (`valid_from`,
//! `valid_to`) so corrections and "stop trusting this fact"
//! invalidations have somewhere to land without rewriting
//! history. `observed_at` is when we first wrote the record;
//! `valid_from` defaults to the same value and is bumped only
//! when a curator splits a span.
//!
//! The store is intentionally separate from the existing
//! `crate::nodes::memory::MemoryStore` (which handles the
//! Hermes-style `turns` table + agent/user memory): the four-
//! layer surface is additive infrastructure, and keeping the
//! schemas separate means each one can evolve independently.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// Which layer of the memory hierarchy a record belongs to.
/// Stored as a short text tag in SQLite so operators can
/// `SELECT * FROM memory_records WHERE layer = 'raw'` from
/// `sqlite3` without joining a vocab table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryLayer {
    Raw,
    Semantic,
    Observation,
    Model,
}

impl MemoryLayer {
    /// Stable wire / column tag. The Display impl mirrors this
    /// so `format!("{layer}")` and `layer.as_str()` agree.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Semantic => "semantic",
            Self::Observation => "observation",
            Self::Model => "model",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "raw" => Some(Self::Raw),
            "semantic" => Some(Self::Semantic),
            "observation" => Some(Self::Observation),
            "model" => Some(Self::Model),
            _ => None,
        }
    }
}

impl std::fmt::Display for MemoryLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// RELIX-7.16: share-policy enum. Stored as the lowercase
/// tag string in the `share_policy` column so operators can
/// `SELECT * FROM memory_records WHERE share_policy = 'auto'`
/// from `sqlite3` without joining a vocab table. `None` is
/// the default (and what every pre-7.16 row migrates to).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharePolicy {
    /// Never shared. Default.
    #[default]
    None,
    /// Shared only when an operator explicitly calls
    /// `knowledge.share` / `knowledge.group_broadcast`.
    Explicit,
    /// Auto-propagated by the AutoShareTask to every other
    /// member of the agent's sharing groups when first
    /// observed.
    Auto,
}

impl SharePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Explicit => "explicit",
            Self::Auto => "auto",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "" => Some(Self::None),
            "explicit" => Some(Self::Explicit),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

/// Source-trust tag stamped on records that enter the memory
/// pipeline. Read by the memory-poisoning quarantine pipeline
/// (GAP 6) — `External` records arriving via ingestion or LLM
/// derivation must be operator-approved before they reach the
/// Layer 3 store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceTrust {
    /// Captured directly from the agent's own conversation
    /// stream. Auto-promoted.
    #[default]
    Internal,
    /// Derived from operator-ingested content (documents,
    /// images, URLs). Quarantined pending operator review.
    External,
    /// Provenance unknown. Treated the same as `External`.
    Unknown,
}

impl SourceTrust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::External => "external",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "internal" | "" => Some(Self::Internal),
            "external" => Some(Self::External),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// One row in the four-layer memory store. `id` is the stable
/// identifier — the embedding pipeline + Qdrant payload both
/// reference it directly so updates are idempotent.
///
/// RELIX-7.16: rows carry four optional sharing fields. They
/// default to safe values (`shareable = false`, `shared_with`
/// empty, `shared_by = None`, `share_policy = None`) so the
/// pre-7.16 pipeline keeps its semantics. The
/// [`MemoryRecord::new_raw`] convenience constructor returns a
/// record with these defaults so existing call sites compile
/// unchanged.
///
/// RELIX-MEM (GAP 6/7/8): rows additionally carry `source_trust`,
/// `frozen`, `last_edited_ms`, and `consolidated` so the
/// quarantine, inspector edit, and archival pipelines all share
/// one schema. Each new column has a safe default and is
/// added to pre-existing databases via the `init_schema`
/// `column_exists`-guarded ALTER pass.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: String,
    pub layer: MemoryLayer,
    pub text: String,
    pub source: String,
    pub tags: Vec<String>,
    pub created_at: i64,
    pub valid_from: i64,
    pub valid_to: Option<i64>,
    pub observed_at: i64,
    pub embedding: Option<Vec<f32>>,
    /// Operator-set flag: this observation is safe to share.
    /// Only meaningful for Layer 3 observations; other layers
    /// keep this `false`.
    pub shareable: bool,
    /// Agent names this record has been explicitly shared
    /// with. Empty on the receiving copy; the SOURCE record
    /// accrues entries as `knowledge.share` /
    /// `knowledge.group_broadcast` runs.
    pub shared_with: Vec<String>,
    /// Source agent name on a copy that was received via
    /// `knowledge.share`. `None` on every original (un-shared)
    /// record.
    pub shared_by: Option<String>,
    /// One of [`SharePolicy::None`] / [`SharePolicy::Explicit`]
    /// / [`SharePolicy::Auto`]. Defaults to `None` so pre-7.16
    /// rows are never auto-propagated.
    pub share_policy: SharePolicy,
    /// GAP 6: provenance trust level. Default `Internal` for
    /// agent-captured records; ingestion paths set `External`.
    #[serde(default)]
    pub source_trust: SourceTrust,
    /// GAP 7: operator-frozen flag. Frozen records are never
    /// overwritten by the curator, never invalidated by context
    /// flush, and never archived by the consolidation pipeline.
    #[serde(default)]
    pub frozen: bool,
    /// GAP 7: unix-ms timestamp of the most recent operator
    /// edit via `memory.edit_record`. `None` on records that
    /// have never been edited.
    #[serde(default)]
    pub last_edited_ms: Option<i64>,
    /// GAP 8: set to true by the consolidation archiver when
    /// every observation derived from this raw record has been
    /// archived. Excluded from future RAG retrieval; preserved
    /// for audit.
    #[serde(default)]
    pub consolidated: bool,
    /// GAP 23: tenant identifier this record belongs to. When
    /// `[memory.qdrant] tenant_isolation = true` the embedder
    /// pipeline upserts the record's vector into the
    /// per-tenant collection derived from this field. `None`
    /// means "default tenant" and keeps single-tenant
    /// deployments byte-identical to pre-GAP-23 behaviour.
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// GAP 18: id of the record that supersedes this one.
    /// `None` on the current head; populated when
    /// [`LayeredMemoryStore::supersede`] retires this record in
    /// favour of a new one (typically a contradicting Layer-3
    /// observation). The pair forms a bi-temporal chain — the
    /// retired row keeps its original `valid_from`, gets
    /// `valid_to = now`, and points forward; the new row
    /// inherits `valid_from = now` and has
    /// `superseded_by = None` until it too is superseded.
    #[serde(default)]
    pub superseded_by: Option<String>,
}

impl MemoryRecord {
    /// Convenience constructor for a raw record about to be
    /// inserted: `created_at = valid_from = observed_at = now`,
    /// `valid_to = None`, `embedding = None`. Tests and the
    /// memory.write_turn hook both use this.
    pub fn new_raw(
        id: impl Into<String>,
        text: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        let now = unix_secs();
        Self {
            id: id.into(),
            layer: MemoryLayer::Raw,
            text: text.into(),
            source: source.into(),
            tags: Vec::new(),
            created_at: now,
            valid_from: now,
            valid_to: None,
            observed_at: now,
            embedding: None,
            shareable: false,
            shared_with: Vec::new(),
            shared_by: None,
            share_policy: SharePolicy::None,
            source_trust: SourceTrust::Internal,
            frozen: false,
            last_edited_ms: None,
            consolidated: false,
            tenant_id: None,
            superseded_by: None,
        }
    }
}

/// GAP 6: one row in the memory_quarantine table. Operators
/// list these via `memory.quarantine_list` and either approve
/// (re-inserting the original record into the main store) or
/// reject (permanently discarding).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuarantineRow {
    pub id: String,
    pub record_json: String,
    pub reason: String,
    pub queued_at_ms: i64,
    pub source_trust: SourceTrust,
}

/// Errors raised by [`LayeredMemoryStore`]. Mirrors the shape
/// of the existing `MemoryError` enum so the memory node's
/// handler glue can wrap either source uniformly.
#[derive(Debug, thiserror::Error)]
pub enum LayeredMemoryError {
    #[error("layered memory db: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("layered memory io: {0}")]
    Io(String),
    #[error("layered memory lock poisoned")]
    Lock,
    #[error("layered memory serialization: {0}")]
    Serialization(String),
    /// PART 4: returned by tenant-aware methods when the
    /// store was opened with `tenant_isolation = true` but
    /// the caller did not supply a tenant id.
    #[error("layered memory: tenant_id required in multi-tenant mode")]
    MissingTenant,
}

/// SQLite-backed store for the four-layer schema. The
/// connection is wrapped in `Arc<Mutex<_>>` because rusqlite's
/// `Connection` is not `Sync`; the memory node's handlers run
/// concurrently.
#[derive(Clone)]
pub struct LayeredMemoryStore {
    conn: Arc<Mutex<Connection>>,
    /// PART 4: when `true`, the tenant-aware variants
    /// (`text_search_for_tenant`, `list_for_tenant`,
    /// `get_for_tenant`) fail closed on a missing tenant id
    /// AND every read query is filtered with
    /// `WHERE tenant_id = ?`. The pre-PART-4 tenant-blind
    /// methods (`text_search`, `list`, `get`) still exist for
    /// callers that have not yet been migrated and for
    /// internal maintenance paths (promoter, consolidator,
    /// migration); they continue to ignore tenant.
    tenant_isolation: bool,
}

impl LayeredMemoryStore {
    /// Open the store at `path`. Creates the parent directory
    /// and the table if absent; applies the project-wide
    /// SQLite pragmas (WAL, foreign_keys, busy timeout).
    /// Tenant isolation defaults to OFF; callers opt in via
    /// [`Self::open_with_tenant_isolation`].
    pub fn open(path: &Path) -> Result<Self, LayeredMemoryError> {
        Self::open_with_tenant_isolation(path, false)
    }

    /// PART 4: open variant that toggles per-tenant
    /// fail-closed filtering on the tenant-aware read methods.
    pub fn open_with_tenant_isolation(
        path: &Path,
        tenant_isolation: bool,
    ) -> Result<Self, LayeredMemoryError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| LayeredMemoryError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::log_integrity_warning(&conn, "memory-layered");
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            tenant_isolation,
        })
    }

    /// In-memory store for tests. Skips the WAL pragma (no-op
    /// on `:memory:` anyway) but keeps foreign_keys + busy
    /// timeout consistent with the file-backed open path.
    pub fn in_memory() -> Result<Self, LayeredMemoryError> {
        Self::in_memory_with_tenant_isolation(false)
    }

    /// PART 4: in-memory variant with explicit isolation
    /// toggle, used by tenant-isolation tests.
    pub fn in_memory_with_tenant_isolation(
        tenant_isolation: bool,
    ) -> Result<Self, LayeredMemoryError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            tenant_isolation,
        })
    }

    /// `true` when this store enforces per-tenant filtering on
    /// the `*_for_tenant` read methods.
    pub fn tenant_isolation_enabled(&self) -> bool {
        self.tenant_isolation
    }

    /// Insert a new record. Idempotent on `id` collision via
    /// `INSERT OR REPLACE` so the caller can re-insert a
    /// record without checking existence first (the memory
    /// pipeline relies on this when re-processing a batch).
    pub fn insert(&self, record: &MemoryRecord) -> Result<(), LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let tags_json = serde_json::to_string(&record.tags)
            .map_err(|e| LayeredMemoryError::Serialization(e.to_string()))?;
        let shared_with_json = if record.shared_with.is_empty() {
            None
        } else {
            Some(
                serde_json::to_string(&record.shared_with)
                    .map_err(|e| LayeredMemoryError::Serialization(e.to_string()))?,
            )
        };
        let embedding_blob = record.embedding.as_ref().map(|v| encode_f32_le(v));
        conn.execute(
            "INSERT OR REPLACE INTO memory_records \
             (id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
              shareable, shared_with, shared_by, share_policy, \
              source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                record.id,
                record.layer.as_str(),
                record.text,
                record.source,
                tags_json,
                record.created_at,
                record.valid_from,
                record.valid_to,
                record.observed_at,
                embedding_blob,
                record.shareable as i32,
                shared_with_json,
                record.shared_by,
                record.share_policy.as_str(),
                record.source_trust.as_str(),
                record.frozen as i32,
                record.last_edited_ms,
                record.consolidated as i32,
                record.tenant_id,
                record.superseded_by,
            ],
        )?;
        Ok(())
    }

    /// Replace the embedding column for an existing record.
    /// Used by the embedding pipeline once it has the vector
    /// from the AI peer.
    pub fn update_embedding(
        &self,
        id: &str,
        embedding: Vec<f32>,
    ) -> Result<(), LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let blob = encode_f32_le(&embedding);
        conn.execute(
            "UPDATE memory_records SET embedding = ?1 WHERE id = ?2",
            params![blob, id],
        )?;
        Ok(())
    }

    /// Replace the `text` column for an existing record. Used
    /// by the RELIX-7.15 PII defense-in-depth pass in the
    /// embedding pipeline: when the anonymizer is enabled and
    /// a row arrived via a bypass path, the pipeline scrubs
    /// the text in-place BEFORE handing it to the embed
    /// function so the Qdrant payload never carries raw PII.
    pub fn update_text(&self, id: &str, text: &str) -> Result<(), LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        conn.execute(
            "UPDATE memory_records SET text = ?1 WHERE id = ?2",
            params![text, id],
        )?;
        Ok(())
    }

    /// RELIX-7.15 bulk-anonymize walker. Walks every row in
    /// `memory_records` and rewrites each row's `text` through
    /// the supplied anonymizer when the result differs.
    /// Returns per-layer (scanned, changed) counts.
    ///
    /// The walker is idempotent: running it twice on the same
    /// store produces zero changes on the second pass because
    /// the anonymizer's placeholders / pseudonyms don't match
    /// the PII patterns. Safe to invoke from operator surfaces
    /// at any time.
    ///
    /// Streams the table id-by-id under a single SQLite lock
    /// so concurrent writes during the walk are linearised. On
    /// a 10k-row store this finishes in a few hundred
    /// milliseconds; operators bulk-anonymizing larger stores
    /// should run the cap during a quiet window.
    pub fn bulk_anonymize_records(
        &self,
        anon: &crate::training::PiiAnonymizer,
    ) -> Result<BulkAnonymizeRecordsStats, LayeredMemoryError> {
        let mut stats = BulkAnonymizeRecordsStats::default();
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut stmt = conn.prepare("SELECT id, layer, text FROM memory_records")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut update_stmt = conn.prepare("UPDATE memory_records SET text = ?1 WHERE id = ?2")?;
        for (id, layer_str, text) in rows {
            let layer = MemoryLayer::parse(&layer_str).unwrap_or(MemoryLayer::Raw);
            let counter = stats.counter_mut(layer);
            counter.scanned += 1;
            // When the anonymizer is disabled it pass-throughs;
            // we only count `changed` for rows where the text
            // actually mutated (so the caller can tell whether
            // a row was already clean vs needed scrubbing).
            let scrubbed = if anon.enabled() {
                anon.anonymize(&text)
            } else {
                text.clone()
            };
            if scrubbed != text {
                update_stmt.execute(params![scrubbed, id])?;
                counter.changed += 1;
            }
        }
        Ok(stats)
    }
}

/// Per-layer counters returned by
/// [`LayeredMemoryStore::bulk_anonymize_records`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BulkAnonymizeRecordsStats {
    pub raw: LayerCount,
    pub semantic: LayerCount,
    pub observation: LayerCount,
    pub model: LayerCount,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LayerCount {
    pub scanned: u64,
    pub changed: u64,
}

impl BulkAnonymizeRecordsStats {
    /// Sum of `scanned` across every layer.
    pub fn total_scanned(&self) -> u64 {
        self.raw.scanned + self.semantic.scanned + self.observation.scanned + self.model.scanned
    }

    /// Sum of `changed` across every layer.
    pub fn total_changed(&self) -> u64 {
        self.raw.changed + self.semantic.changed + self.observation.changed + self.model.changed
    }

    fn counter_mut(&mut self, layer: MemoryLayer) -> &mut LayerCount {
        match layer {
            MemoryLayer::Raw => &mut self.raw,
            MemoryLayer::Semantic => &mut self.semantic,
            MemoryLayer::Observation => &mut self.observation,
            MemoryLayer::Model => &mut self.model,
        }
    }
}

impl LayeredMemoryStore {
    /// Fetch one record by id. `Ok(None)` when the row is
    /// absent; never raises.
    pub fn get(&self, id: &str) -> Result<Option<MemoryRecord>, LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let row = conn
            .query_row(
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records WHERE id = ?1",
                params![id],
                row_to_record,
            )
            .optional()?;
        row.transpose()
    }

    /// PART 2: batch fetch by id. Folds an N+1 of `get(id)` into
    /// a single `WHERE id IN (?, ?, …)` lookup. Returns a map
    /// keyed by id; absent ids simply do not appear, mirroring
    /// `get` returning `None`. Input is chunked at 500 ids to
    /// stay under SQLite's per-statement parameter cap across
    /// hosts. Iteration order is irrelevant — callers index by id.
    pub fn get_many(
        &self,
        ids: &[&str],
    ) -> Result<std::collections::HashMap<String, MemoryRecord>, LayeredMemoryError> {
        let mut out = std::collections::HashMap::with_capacity(ids.len());
        if ids.is_empty() {
            return Ok(out);
        }
        const CHUNK: usize = 500;
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        for chunk in ids.chunks(CHUNK) {
            let placeholders: String = (0..chunk.len())
                .map(|i| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records WHERE id IN ({placeholders})"
            );
            let bind: Vec<rusqlite::types::Value> = chunk
                .iter()
                .map(|id| rusqlite::types::Value::Text((*id).to_string()))
                .collect();
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(bind.iter()), row_to_record)?;
            for r in rows {
                let rec = r??;
                out.insert(rec.id.clone(), rec);
            }
        }
        Ok(out)
    }

    /// Paginated list of records. Filtered optionally by
    /// `layer` and `source`. Ordered by `created_at DESC` for
    /// "most recent first" operator surfaces; ties broken on
    /// `id ASC` so test ordering is deterministic.
    pub fn list(
        &self,
        layer: Option<MemoryLayer>,
        source: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<MemoryRecord>, LayeredMemoryError> {
        let limit = limit.clamp(1, 1000) as i64;
        let offset = offset as i64;
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let (sql, params_vec): (&str, Vec<rusqlite::types::Value>) = match (layer, source) {
            (None, None) => (
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records \
                 ORDER BY created_at DESC, id ASC \
                 LIMIT ?1 OFFSET ?2",
                vec![limit.into(), offset.into()],
            ),
            (Some(l), None) => (
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records WHERE layer = ?3 \
                 ORDER BY created_at DESC, id ASC \
                 LIMIT ?1 OFFSET ?2",
                vec![limit.into(), offset.into(), l.as_str().to_string().into()],
            ),
            (None, Some(s)) => (
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records WHERE source = ?3 \
                 ORDER BY created_at DESC, id ASC \
                 LIMIT ?1 OFFSET ?2",
                vec![limit.into(), offset.into(), s.to_string().into()],
            ),
            (Some(l), Some(s)) => (
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records WHERE layer = ?3 AND source = ?4 \
                 ORDER BY created_at DESC, id ASC \
                 LIMIT ?1 OFFSET ?2",
                vec![
                    limit.into(),
                    offset.into(),
                    l.as_str().to_string().into(),
                    s.to_string().into(),
                ],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params_vec.iter()), |r| {
            row_to_record(r)
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// CORR PART 6: cursor-based variant of [`Self::list`].
    /// Returns up to `limit` rows whose SQLite rowid is
    /// strictly greater than `cursor_rowid`, paired with the
    /// rowid so the caller can mint the next cursor. Ordered
    /// `rowid ASC` so a stable forward scan is possible — the
    /// pre-fix `ORDER BY created_at DESC` ordering doesn't
    /// support cursoring (two rows can share a created_at,
    /// and DESC doesn't compose with `> cursor` semantics).
    pub fn list_after_rowid(
        &self,
        layer: Option<MemoryLayer>,
        source: Option<&str>,
        cursor_rowid: i64,
        limit: i64,
    ) -> Result<Vec<(i64, MemoryRecord)>, LayeredMemoryError> {
        let limit = limit.clamp(1, 4096);
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let (sql, params_vec): (&str, Vec<rusqlite::types::Value>) = match (layer, source) {
            (None, None) => (
                "SELECT rowid, id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records \
                 WHERE rowid > ?2 \
                 ORDER BY rowid ASC \
                 LIMIT ?1",
                vec![limit.into(), cursor_rowid.into()],
            ),
            (Some(l), None) => (
                "SELECT rowid, id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records WHERE layer = ?3 AND rowid > ?2 \
                 ORDER BY rowid ASC \
                 LIMIT ?1",
                vec![
                    limit.into(),
                    cursor_rowid.into(),
                    l.as_str().to_string().into(),
                ],
            ),
            (None, Some(s)) => (
                "SELECT rowid, id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records WHERE source = ?3 AND rowid > ?2 \
                 ORDER BY rowid ASC \
                 LIMIT ?1",
                vec![limit.into(), cursor_rowid.into(), s.to_string().into()],
            ),
            (Some(l), Some(s)) => (
                "SELECT rowid, id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records WHERE layer = ?3 AND source = ?4 AND rowid > ?2 \
                 ORDER BY rowid ASC \
                 LIMIT ?1",
                vec![
                    limit.into(),
                    cursor_rowid.into(),
                    l.as_str().to_string().into(),
                    s.to_string().into(),
                ],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params_vec.iter()), |r| {
            let rowid: i64 = r.get(0)?;
            // Skip the rowid column for `row_to_record`; the
            // *_offset variant reads from index 1 onward.
            Ok((rowid, row_to_record_offset(r)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (rowid, rec_res) = r?;
            out.push((rowid, rec_res?));
        }
        Ok(out)
    }

    /// Naive substring search against the `text` column. Used
    /// by `memory.search` when no Qdrant is configured. Case-
    /// insensitive (SQLite's `LIKE` is case-insensitive for
    /// ASCII by default). Ordered newest-first.
    pub fn text_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, LayeredMemoryError> {
        let limit = limit.clamp(1, 1000) as i64;
        let pattern = format!("%{}%", query.replace('%', "\\%"));
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
             FROM memory_records WHERE text LIKE ?1 ESCAPE '\\' \
             ORDER BY created_at DESC, id ASC \
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit], row_to_record)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// PART 4: tenant-aware variant of [`text_search`]. Adds a
    /// `WHERE tenant_id = ?1` clause so the SQLite fallback
    /// path cannot leak rows across tenants. When
    /// `tenant_isolation = true` AND `tenant_id` is `None` /
    /// empty, returns
    /// [`LayeredMemoryError::MissingTenant`].
    ///
    /// When `tenant_isolation = false`, this method falls
    /// through to [`Self::text_search`] (no filter applied)
    /// so single-tenant deployments stay byte-identical.
    pub fn text_search_for_tenant(
        &self,
        query: &str,
        limit: usize,
        tenant_id: Option<&str>,
    ) -> Result<Vec<MemoryRecord>, LayeredMemoryError> {
        if !self.tenant_isolation {
            return self.text_search(query, limit);
        }
        let tenant = match tenant_id {
            Some(t) if !t.trim().is_empty() => t,
            _ => return Err(LayeredMemoryError::MissingTenant),
        };
        let limit = limit.clamp(1, 1000) as i64;
        let pattern = format!("%{}%", query.replace('%', "\\%"));
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
             FROM memory_records \
             WHERE tenant_id = ?1 AND text LIKE ?2 ESCAPE '\\' \
             ORDER BY created_at DESC, id ASC \
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![tenant, pattern, limit], row_to_record)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// PART 4: tenant-aware variant of [`Self::get`]. Returns
    /// the row only when its `tenant_id` matches; returns
    /// `Ok(None)` (not an error) when the row exists but
    /// belongs to a different tenant — a leaked id must not
    /// reveal cross-tenant existence.
    pub fn get_for_tenant(
        &self,
        id: &str,
        tenant_id: Option<&str>,
    ) -> Result<Option<MemoryRecord>, LayeredMemoryError> {
        if !self.tenant_isolation {
            return self.get(id);
        }
        let tenant = match tenant_id {
            Some(t) if !t.trim().is_empty() => t,
            _ => return Err(LayeredMemoryError::MissingTenant),
        };
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
             FROM memory_records \
             WHERE id = ?1 AND tenant_id = ?2",
        )?;
        let mut rows = stmt.query_map(params![id, tenant], row_to_record)?;
        match rows.next() {
            Some(r) => Ok(Some(r??)),
            None => Ok(None),
        }
    }

    /// Mark a record as no longer valid by stamping
    /// `valid_to`. Idempotent — calling on an already-
    /// invalidated row is a no-op (the second value just
    /// overwrites the first).
    pub fn invalidate(&self, id: &str, at: i64) -> Result<(), LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        conn.execute(
            "UPDATE memory_records SET valid_to = ?1 WHERE id = ?2",
            params![at, id],
        )?;
        Ok(())
    }

    /// GAP 18: bi-temporal supersede. Retires `old_id` at
    /// timestamp `at` (stamps `valid_to = at` AND
    /// `superseded_by = new.id`) and inserts `new_record` as
    /// the new head. The two writes happen inside one SQLite
    /// transaction so a crash mid-supersede never leaves the
    /// chain in a half-retired state.
    ///
    /// Use this instead of [`Self::insert`] + [`Self::invalidate`]
    /// when a fact has *changed* and the host wants the audit
    /// trail to preserve the prior assertion. The
    /// [`Self::as_of`] helper relies on the
    /// `(valid_from, valid_to)` interval to surface the
    /// correct historical row.
    ///
    /// Returns `Err(LayeredMemoryError::Serialization)` when
    /// the old record does not exist; this surfaces config
    /// bugs (writing a supersede against a vanished row)
    /// rather than silently inserting an orphan.
    pub fn supersede(
        &self,
        old_id: &str,
        new_record: &MemoryRecord,
        at: i64,
    ) -> Result<(), LayeredMemoryError> {
        let mut conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let tx = conn.transaction()?;
        let updated = tx.execute(
            "UPDATE memory_records \
             SET valid_to = ?1, superseded_by = ?2 \
             WHERE id = ?3",
            params![at, new_record.id, old_id],
        )?;
        if updated == 0 {
            return Err(LayeredMemoryError::Serialization(format!(
                "supersede target {old_id:?} does not exist"
            )));
        }
        let tags_json = serde_json::to_string(&new_record.tags)
            .map_err(|e| LayeredMemoryError::Serialization(e.to_string()))?;
        let shared_with_json = if new_record.shared_with.is_empty() {
            None
        } else {
            Some(
                serde_json::to_string(&new_record.shared_with)
                    .map_err(|e| LayeredMemoryError::Serialization(e.to_string()))?,
            )
        };
        let embedding_blob = new_record.embedding.as_ref().map(|v| encode_f32_le(v));
        tx.execute(
            "INSERT OR REPLACE INTO memory_records \
             (id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
              shareable, shared_with, shared_by, share_policy, \
              source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            params![
                new_record.id,
                new_record.layer.as_str(),
                new_record.text,
                new_record.source,
                tags_json,
                new_record.created_at,
                new_record.valid_from,
                new_record.valid_to,
                new_record.observed_at,
                embedding_blob,
                new_record.shareable as i32,
                shared_with_json,
                new_record.shared_by,
                new_record.share_policy.as_str(),
                new_record.source_trust.as_str(),
                new_record.frozen as i32,
                new_record.last_edited_ms,
                new_record.consolidated as i32,
                new_record.tenant_id,
                new_record.superseded_by,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// GAP 18: bi-temporal point-in-time read.
    ///
    /// Returns every record whose validity window contains
    /// `at` — i.e. `valid_from <= at AND (valid_to IS NULL OR
    /// valid_to > at)`. Optionally narrows to one `source`
    /// (passing an empty string disables the source filter).
    /// Results are ordered by `observed_at DESC` so the most
    /// recent assertion within the window appears first.
    pub fn as_of(
        &self,
        at: i64,
        source: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, LayeredMemoryError> {
        let limit = limit.clamp(1, 10_000) as i64;
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut out = Vec::new();
        if source.is_empty() {
            let mut stmt = conn.prepare(
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                        shareable, shared_with, shared_by, share_policy, \
                        source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records \
                 WHERE valid_from <= ?1 AND (valid_to IS NULL OR valid_to > ?1) \
                 ORDER BY observed_at DESC, id ASC \
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![at, limit], row_to_record)?;
            for r in rows {
                out.push(r??);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                        shareable, shared_with, shared_by, share_policy, \
                        source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records \
                 WHERE source = ?1 AND valid_from <= ?2 AND (valid_to IS NULL OR valid_to > ?2) \
                 ORDER BY observed_at DESC, id ASC \
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![source, at, limit], row_to_record)?;
            for r in rows {
                out.push(r??);
            }
        };
        Ok(out)
    }

    /// GAP 18: walk the supersedes chain forward from
    /// `start_id`, returning every record id from the start
    /// through the current head. Each step follows the
    /// `superseded_by` pointer until a record with no
    /// successor (the head) is reached or a cycle is
    /// detected. Cycles short-circuit by walk length cap
    /// (1024) so a corrupt chain can't lock the call up.
    pub fn supersedes_chain(&self, start_id: &str) -> Result<Vec<String>, LayeredMemoryError> {
        const MAX_HOPS: usize = 1024;
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut current = start_id.to_string();
        let mut out: Vec<String> = vec![current.clone()];
        for _ in 0..MAX_HOPS {
            let mut stmt =
                conn.prepare("SELECT superseded_by FROM memory_records WHERE id = ?1")?;
            let next: Option<String> = stmt
                .query_row(params![current], |r| r.get::<_, Option<String>>(0))
                .ok()
                .flatten();
            match next {
                Some(n) if !n.is_empty() && n != current => {
                    out.push(n.clone());
                    current = n;
                }
                _ => break,
            }
        }
        Ok(out)
    }

    /// RELIX-7.16 GAP 1: fetch a batch of Layer 3 observations
    /// that don't carry a `quality:<f>` tag yet. Used by the
    /// background `MemoryQualityScorer` task. The records are
    /// ordered ascending by `observed_at` so the scorer makes
    /// monotonic progress across restarts.
    ///
    /// The unscored predicate is a substring check in SQL
    /// (`tags NOT LIKE '%"quality:%'`) which is faster than
    /// deserialising every row in Rust to inspect the tag list.
    /// False positives (e.g. a record whose `text` literally
    /// contains the substring `"quality:` — extraordinarily
    /// rare in practice) are caught + filtered after the
    /// fetch via [`extract_quality_score`]-equivalent logic
    /// in the scorer.
    pub fn fetch_unscored_observations(
        &self,
        limit: u32,
    ) -> Result<Vec<MemoryRecord>, LayeredMemoryError> {
        let limit = limit.clamp(1, 1000) as i64;
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
             FROM memory_records \
             WHERE layer = 'observation' \
               AND tags NOT LIKE '%\"quality:%' \
             ORDER BY observed_at ASC, id ASC \
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_record)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// Append a tag to a record's `tags` JSON array. Used by
    /// the layer promoter to stamp "promoted:<layer>" markers
    /// so the same record isn't re-promoted on the next tick.
    /// No-op when the tag is already present.
    pub fn add_tag(&self, id: &str, tag: &str) -> Result<(), LayeredMemoryError> {
        let current = match self.get(id)? {
            Some(r) => r,
            None => return Ok(()),
        };
        if current.tags.iter().any(|t| t == tag) {
            return Ok(());
        }
        let mut tags = current.tags;
        tags.push(tag.to_string());
        let tags_json = serde_json::to_string(&tags)
            .map_err(|e| LayeredMemoryError::Serialization(e.to_string()))?;
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        conn.execute(
            "UPDATE memory_records SET tags = ?1 WHERE id = ?2",
            params![tags_json, id],
        )?;
        Ok(())
    }

    /// Return the most recent record for `(layer, source)`
    /// regardless of validity. Used by the promoter to throttle
    /// Model regeneration to "at most once per hour per source"
    /// — the caller compares `observed_at` to a cutoff.
    pub fn latest_by_layer_and_source(
        &self,
        layer: MemoryLayer,
        source: &str,
    ) -> Result<Option<MemoryRecord>, LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let row = conn
            .query_row(
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records WHERE layer = ?1 AND source = ?2 \
                 ORDER BY observed_at DESC, id ASC LIMIT 1",
                params![layer.as_str(), source],
                row_to_record,
            )
            .optional()?;
        row.transpose()
    }

    /// Count records with `embedding IS NULL`, grouped by
    /// layer. Used by the embedding pipeline to decide which
    /// layers have work pending.
    pub fn count_pending_embeddings(
        &self,
    ) -> Result<BTreeMap<MemoryLayer, usize>, LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT layer, COUNT(*) FROM memory_records \
             WHERE embedding IS NULL \
             GROUP BY layer",
        )?;
        let rows = stmt.query_map([], |r| {
            let layer: String = r.get(0)?;
            let count: i64 = r.get(1)?;
            Ok((layer, count))
        })?;
        let mut out: BTreeMap<MemoryLayer, usize> = BTreeMap::new();
        for r in rows {
            let (layer_s, count) = r?;
            if let Some(layer) = MemoryLayer::parse(&layer_s) {
                out.insert(layer, count as usize);
            }
        }
        Ok(out)
    }

    /// GAP 7: operator edit — replace `text` and stamp
    /// `last_edited_ms`. Clears the existing embedding so the
    /// background pipeline re-embeds the new text on its next
    /// tick. Frozen rows are still editable — the operator
    /// who froze the row is the same operator editing it.
    pub fn edit_record_text(
        &self,
        id: &str,
        new_text: &str,
        edited_at_ms: i64,
    ) -> Result<(), LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let n = conn.execute(
            "UPDATE memory_records SET text = ?1, last_edited_ms = ?2, embedding = NULL \
             WHERE id = ?3",
            params![new_text, edited_at_ms, id],
        )?;
        if n == 0 {
            return Err(LayeredMemoryError::Serialization(format!(
                "memory record {id} not found"
            )));
        }
        Ok(())
    }

    /// GAP 7: set the `frozen` flag. A frozen record survives
    /// the curator, the context-flush archiver, and the
    /// consolidation pass. Idempotent.
    pub fn set_frozen(&self, id: &str, frozen: bool) -> Result<(), LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let n = conn.execute(
            "UPDATE memory_records SET frozen = ?1 WHERE id = ?2",
            params![frozen as i32, id],
        )?;
        if n == 0 {
            return Err(LayeredMemoryError::Serialization(format!(
                "memory record {id} not found"
            )));
        }
        Ok(())
    }

    /// GAP 8: stamp the consolidated flag. Used by the
    /// archiver to mark raw records whose downstream
    /// observations have all been archived.
    pub fn set_consolidated(&self, id: &str, consolidated: bool) -> Result<(), LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        conn.execute(
            "UPDATE memory_records SET consolidated = ?1 WHERE id = ?2",
            params![consolidated as i32, id],
        )?;
        Ok(())
    }

    /// GAP 7: rewrite the `observed_at` column for one record.
    /// Used by `memory.request_model_refresh` to age the latest
    /// Layer-4 model past the promoter's throttle so the next
    /// curator tick regenerates it on demand.
    pub fn touch_observed_at(&self, id: &str, observed_at: i64) -> Result<(), LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let n = conn.execute(
            "UPDATE memory_records SET observed_at = ?1 WHERE id = ?2",
            params![observed_at, id],
        )?;
        if n == 0 {
            return Err(LayeredMemoryError::Serialization(format!(
                "memory record {id} not found"
            )));
        }
        Ok(())
    }

    /// GAP 7: bulk-export every record for `subject_id`,
    /// optionally filtered to a single layer. Returns the
    /// records as a Vec (bridge wraps as JSON / JSONL).
    pub fn export_for_source(
        &self,
        source: &str,
        layer: Option<MemoryLayer>,
    ) -> Result<Vec<MemoryRecord>, LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let (sql, params_vec): (&str, Vec<rusqlite::types::Value>) = match layer {
            Some(l) => (
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                        shareable, shared_with, shared_by, share_policy, \
                        source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records WHERE source = ?1 AND layer = ?2 \
                 ORDER BY observed_at ASC, id ASC",
                vec![source.to_string().into(), l.as_str().to_string().into()],
            ),
            None => (
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                        shareable, shared_with, shared_by, share_policy, \
                        source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
                 FROM memory_records WHERE source = ?1 \
                 ORDER BY observed_at ASC, id ASC",
                vec![source.to_string().into()],
            ),
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params_vec.iter()), row_to_record)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// GAP 8: list valid observation records older than
    /// `cutoff_observed_at` that are NOT frozen and NOT
    /// archived. Used by the consolidation archiver to find
    /// terminal observations.
    pub fn list_archive_candidates(
        &self,
        layer: MemoryLayer,
        cutoff_observed_at: i64,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, LayeredMemoryError> {
        let limit = limit.clamp(1, 10_000) as i64;
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
             FROM memory_records \
             WHERE layer = ?1 \
               AND frozen = 0 \
               AND valid_to IS NULL \
               AND observed_at < ?2 \
               AND tags NOT LIKE '%\"archived\"%' \
             ORDER BY observed_at ASC, id ASC \
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![layer.as_str(), cutoff_observed_at, limit],
            row_to_record,
        )?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// GAP 8: list raw records whose downstream observations
    /// have ALL been archived. Used by the archiver to mark
    /// the source raw as `consolidated = true`.
    pub fn list_raw_candidates_for_consolidation(
        &self,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, LayeredMemoryError> {
        let limit = limit.clamp(1, 10_000) as i64;
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
             FROM memory_records \
             WHERE layer = 'raw' \
               AND consolidated = 0 \
               AND frozen = 0 \
             ORDER BY observed_at ASC, id ASC \
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_record)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// GAP 6: integrity audit — count records flagged by the
    /// auditor. Each return field is the count of records the
    /// auditor stamped with the corresponding tag during the
    /// last sweep. Used by tests + the status endpoint.
    pub fn count_records_with_tag(&self, tag: &str) -> Result<u64, LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let needle = format!("%\"{tag}\"%");
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memory_records WHERE tags LIKE ?1 ESCAPE '\\'",
            params![needle],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }

    /// GAP 6: list every valid Layer 3 observation that's
    /// older than `cutoff_observed_at` and has no source
    /// attribution. Used by the integrity auditor.
    pub fn list_observations_missing_source(
        &self,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, LayeredMemoryError> {
        let limit = limit.clamp(1, 10_000) as i64;
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
             FROM memory_records \
             WHERE layer = 'observation' \
               AND (source IS NULL OR source = '') \
             ORDER BY observed_at ASC, id ASC \
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_record)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// GAP 6: list stale unmodeled Layer 3 observations —
    /// records that have been in the store for at least
    /// `min_age_secs` without being referenced by any Layer 4
    /// model update for their source.
    pub fn list_stale_unmodeled_observations(
        &self,
        cutoff_observed_at: i64,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, LayeredMemoryError> {
        let limit = limit.clamp(1, 10_000) as i64;
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
             FROM memory_records r \
             WHERE r.layer = 'observation' \
               AND r.observed_at < ?1 \
               AND r.valid_to IS NULL \
               AND NOT EXISTS ( \
                   SELECT 1 FROM memory_records m \
                   WHERE m.layer = 'model' \
                     AND m.source = r.source \
                     AND m.observed_at > r.observed_at \
               ) \
             ORDER BY r.observed_at ASC, r.id ASC \
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![cutoff_observed_at, limit], row_to_record)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// GAP 6: list Layer 4 model records whose `source` has
    /// no corresponding Layer 3 observation rows. These
    /// "models without sources" are flagged by the auditor.
    pub fn list_unsourced_models(
        &self,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, LayeredMemoryError> {
        let limit = limit.clamp(1, 10_000) as i64;
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
             FROM memory_records m \
             WHERE m.layer = 'model' \
               AND m.valid_to IS NULL \
               AND NOT EXISTS ( \
                   SELECT 1 FROM memory_records o \
                   WHERE o.layer = 'observation' \
                     AND o.source = m.source \
               ) \
             ORDER BY m.observed_at ASC, m.id ASC \
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_record)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// GAP 6: push a quarantine row. `record_json` is the
    /// serialised candidate; the caller can rehydrate it via
    /// `serde_json::from_str` after operator approval.
    pub fn quarantine_insert(
        &self,
        id: &str,
        record_json: &str,
        reason: &str,
        queued_at_ms: i64,
        source_trust: SourceTrust,
    ) -> Result<(), LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        conn.execute(
            "INSERT OR REPLACE INTO memory_quarantine \
             (id, record_json, reason, queued_at_ms, source_trust) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, record_json, reason, queued_at_ms, source_trust.as_str()],
        )?;
        Ok(())
    }

    /// GAP 6: list the pending quarantine rows, newest first.
    pub fn quarantine_list(&self, limit: usize) -> Result<Vec<QuarantineRow>, LayeredMemoryError> {
        let limit = limit.clamp(1, 1000) as i64;
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT id, record_json, reason, queued_at_ms, source_trust \
             FROM memory_quarantine \
             ORDER BY queued_at_ms DESC, id ASC \
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(QuarantineRow {
                id: r.get(0)?,
                record_json: r.get(1)?,
                reason: r.get(2)?,
                queued_at_ms: r.get(3)?,
                source_trust: r
                    .get::<_, String>(4)
                    .ok()
                    .as_deref()
                    .and_then(SourceTrust::parse)
                    .unwrap_or(SourceTrust::Unknown),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// GAP 6: pop one quarantine row by id. Returns the JSON
    /// payload so the caller can rehydrate the candidate and
    /// re-insert it into the main store on approval.
    pub fn quarantine_take(&self, id: &str) -> Result<Option<QuarantineRow>, LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let row = conn
            .query_row(
                "SELECT id, record_json, reason, queued_at_ms, source_trust \
                 FROM memory_quarantine WHERE id = ?1",
                params![id],
                |r| {
                    Ok(QuarantineRow {
                        id: r.get(0)?,
                        record_json: r.get(1)?,
                        reason: r.get(2)?,
                        queued_at_ms: r.get(3)?,
                        source_trust: r
                            .get::<_, String>(4)
                            .ok()
                            .as_deref()
                            .and_then(SourceTrust::parse)
                            .unwrap_or(SourceTrust::Unknown),
                    })
                },
            )
            .optional()?;
        if row.is_some() {
            conn.execute("DELETE FROM memory_quarantine WHERE id = ?1", params![id])?;
        }
        Ok(row)
    }

    /// GAP 6: clear a quarantine row without re-inserting it
    /// into the main store. Used by `memory.quarantine_reject`.
    pub fn quarantine_delete(&self, id: &str) -> Result<bool, LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let n = conn.execute("DELETE FROM memory_quarantine WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Return up to `limit` records that still need embedding.
    /// Ordered by `observed_at ASC` so the oldest unembedded
    /// record gets processed first. Used by the embedding
    /// pipeline to fill a batch.
    pub fn fetch_pending_embeddings(
        &self,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, LayeredMemoryError> {
        let limit = limit.clamp(1, 1000) as i64;
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding, \
                    shareable, shared_with, shared_by, share_policy, \
                    source_trust, frozen, last_edited_ms, consolidated, tenant_id, superseded_by \
             FROM memory_records \
             WHERE embedding IS NULL \
             ORDER BY observed_at ASC, id ASC \
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_record)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }
}

fn init_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    // Step 1: create the base table (with the 7.16 + RELIX-MEM
    // columns baked in) + the legacy indexes. On a pre-7.16
    // database the CREATE TABLE IF NOT EXISTS is a no-op; the
    // share + memory-gap columns get backfilled by the ALTER
    // pass below.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_records (\
             id              TEXT PRIMARY KEY,\
             layer           TEXT NOT NULL,\
             text            TEXT NOT NULL,\
             source          TEXT NOT NULL DEFAULT '',\
             tags            TEXT NOT NULL DEFAULT '[]',\
             created_at      INTEGER NOT NULL,\
             valid_from      INTEGER NOT NULL,\
             valid_to        INTEGER,\
             observed_at     INTEGER NOT NULL,\
             embedding       BLOB,\
             shareable       INTEGER NOT NULL DEFAULT 0,\
             shared_with     TEXT,\
             shared_by       TEXT,\
             share_policy    TEXT NOT NULL DEFAULT 'none',\
             source_trust    TEXT NOT NULL DEFAULT 'internal',\
             frozen          INTEGER NOT NULL DEFAULT 0,\
             last_edited_ms  INTEGER,\
             consolidated    INTEGER NOT NULL DEFAULT 0\
         );\
         CREATE INDEX IF NOT EXISTS memory_records_layer_created \
             ON memory_records(layer, created_at DESC);\
         CREATE INDEX IF NOT EXISTS memory_records_source \
             ON memory_records(source);\
         CREATE INDEX IF NOT EXISTS memory_records_pending \
             ON memory_records(observed_at) WHERE embedding IS NULL;",
    )?;
    // Step 2: RELIX-7.16 backwards-compat migration. ALTER
    // TABLE ADD COLUMN guarded by PRAGMA table_info probe. A
    // pre-7.16 database opening against this build picks up
    // the four new columns without losing any rows.
    if !column_exists(conn, "memory_records", "shareable")? {
        conn.execute_batch(
            "ALTER TABLE memory_records ADD COLUMN shareable INTEGER NOT NULL DEFAULT 0;\
             ALTER TABLE memory_records ADD COLUMN shared_with TEXT;\
             ALTER TABLE memory_records ADD COLUMN shared_by TEXT;\
             ALTER TABLE memory_records ADD COLUMN share_policy TEXT NOT NULL DEFAULT 'none';",
        )?;
    }
    // RELIX-MEM (GAP 6/7/8): backwards-compat migration for
    // the source-trust / freeze / edit / archive columns.
    // Each column is added independently so a pre-existing
    // store that has a subset of these (e.g. from a partial
    // intermediate build) still migrates cleanly.
    if !column_exists(conn, "memory_records", "source_trust")? {
        conn.execute(
            "ALTER TABLE memory_records ADD COLUMN source_trust TEXT NOT NULL DEFAULT 'internal'",
            [],
        )?;
    }
    if !column_exists(conn, "memory_records", "frozen")? {
        conn.execute(
            "ALTER TABLE memory_records ADD COLUMN frozen INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    if !column_exists(conn, "memory_records", "last_edited_ms")? {
        conn.execute(
            "ALTER TABLE memory_records ADD COLUMN last_edited_ms INTEGER",
            [],
        )?;
    }
    if !column_exists(conn, "memory_records", "consolidated")? {
        conn.execute(
            "ALTER TABLE memory_records ADD COLUMN consolidated INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    // GAP 23: per-tenant column. NULL means "default tenant"
    // — operator can flip `[memory.qdrant] tenant_isolation
    // = true` on an existing store and the migration leaves
    // every prior row in the default collection.
    if !column_exists(conn, "memory_records", "tenant_id")? {
        conn.execute("ALTER TABLE memory_records ADD COLUMN tenant_id TEXT", [])?;
    }
    // GAP 18: bi-temporal supersedes pointer. NULL on the
    // current head of every fact chain; non-NULL on retired
    // rows pointing forward to the row that replaced them.
    // Pre-7.34 databases get a clean migration — every prior
    // row is treated as a current head until / unless an
    // explicit supersede call retires it.
    if !column_exists(conn, "memory_records", "superseded_by")? {
        conn.execute(
            "ALTER TABLE memory_records ADD COLUMN superseded_by TEXT",
            [],
        )?;
    }
    // Step 3: 7.16 indexes. Always issued with IF NOT EXISTS
    // so they're idempotent across both fresh and migrated
    // databases.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS memory_records_share_policy \
             ON memory_records(share_policy) WHERE share_policy != 'none';\
         CREATE INDEX IF NOT EXISTS memory_records_shared_by \
             ON memory_records(shared_by) WHERE shared_by IS NOT NULL;\
         CREATE INDEX IF NOT EXISTS idx_memory_records_tenant \
             ON memory_records(tenant_id) WHERE tenant_id IS NOT NULL;",
    )?;
    // RELIX-MEM (GAP 6): quarantine table for high-anomaly
    // observations + external-trust records awaiting operator
    // approval. Stored as a separate table rather than a
    // boolean column so the main memory_records query path
    // stays linear — the quarantine is poll-by-list,
    // approve-or-reject, not part of normal retrieval.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_quarantine (\
             id            TEXT PRIMARY KEY,\
             record_json   TEXT NOT NULL,\
             reason        TEXT NOT NULL,\
             queued_at_ms  INTEGER NOT NULL,\
             source_trust  TEXT NOT NULL DEFAULT 'unknown'\
         );\
         CREATE INDEX IF NOT EXISTS memory_quarantine_queued_at \
             ON memory_quarantine(queued_at_ms DESC);",
    )?;
    // RELIX-MEM (GAP 8): indexes for the consolidation
    // archiver's hot path (age + confidence scan).
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS memory_records_archive_scan \
             ON memory_records(layer, observed_at) \
             WHERE frozen = 0 AND valid_to IS NULL;",
    )?;
    Ok(())
}

/// Probe `PRAGMA table_info` for `column` on `table`. Used by
/// the RELIX-7.16 migration path to make column-add idempotent
/// (SQLite has no `ADD COLUMN IF NOT EXISTS`).
fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for r in rows {
        if r? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `SELECT` row → `Result<MemoryRecord, LayeredMemoryError>`,
/// folded through rusqlite's `Result` so we can use it inside
/// `query_map`. The outer `Result<_>` is rusqlite's; the inner
/// `Result<_>` is ours (returns the parse error on bad rows).
/// CORR PART 6: variant of [`row_to_record`] that reads from
/// index 1 onward, leaving index 0 for the caller's rowid
/// projection. Used by [`LayeredMemoryStore::list_after_rowid`].
fn row_to_record_offset(
    r: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<MemoryRecord, LayeredMemoryError>> {
    let id: String = r.get(1)?;
    let layer_s: String = r.get(2)?;
    let text: String = r.get(3)?;
    let source: String = r.get(4)?;
    let tags_json: String = r.get(5)?;
    let created_at: i64 = r.get(6)?;
    let valid_from: i64 = r.get(7)?;
    let valid_to: Option<i64> = r.get(8)?;
    let observed_at: i64 = r.get(9)?;
    let embedding_blob: Option<Vec<u8>> = r.get(10)?;
    let shareable: i32 = r.get(11).unwrap_or(0);
    let shared_with_json: Option<String> = r.get(12).unwrap_or(None);
    let shared_by: Option<String> = r.get(13).unwrap_or(None);
    let share_policy_s: Option<String> = r.get(14).unwrap_or(None);
    let source_trust_s: Option<String> = r.get(15).unwrap_or(None);
    let frozen: i32 = r.get(16).unwrap_or(0);
    let last_edited_ms: Option<i64> = r.get(17).unwrap_or(None);
    let consolidated: i32 = r.get(18).unwrap_or(0);
    let tenant_id: Option<String> = r.get(19).unwrap_or(None);
    let superseded_by: Option<String> = r.get(20).unwrap_or(None);
    Ok((|| {
        let layer = MemoryLayer::parse(&layer_s).ok_or_else(|| {
            LayeredMemoryError::Serialization(format!("unknown layer: {layer_s}"))
        })?;
        let tags: Vec<String> = serde_json::from_str(&tags_json)
            .map_err(|e| LayeredMemoryError::Serialization(e.to_string()))?;
        let embedding = embedding_blob.map(|b| decode_f32_le(&b));
        let shared_with: Vec<String> = match shared_with_json.as_deref() {
            None | Some("") => Vec::new(),
            Some(s) => serde_json::from_str(s).unwrap_or_default(),
        };
        let share_policy = share_policy_s
            .as_deref()
            .and_then(SharePolicy::parse)
            .unwrap_or(SharePolicy::None);
        let source_trust = source_trust_s
            .as_deref()
            .and_then(SourceTrust::parse)
            .unwrap_or(SourceTrust::Internal);
        Ok(MemoryRecord {
            id,
            layer,
            text,
            source,
            tags,
            created_at,
            valid_from,
            valid_to,
            observed_at,
            embedding,
            shareable: shareable != 0,
            shared_with,
            shared_by,
            share_policy,
            source_trust,
            frozen: frozen != 0,
            last_edited_ms,
            consolidated: consolidated != 0,
            tenant_id,
            superseded_by,
        })
    })())
}

fn row_to_record(
    r: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<MemoryRecord, LayeredMemoryError>> {
    let id: String = r.get(0)?;
    let layer_s: String = r.get(1)?;
    let text: String = r.get(2)?;
    let source: String = r.get(3)?;
    let tags_json: String = r.get(4)?;
    let created_at: i64 = r.get(5)?;
    let valid_from: i64 = r.get(6)?;
    let valid_to: Option<i64> = r.get(7)?;
    let observed_at: i64 = r.get(8)?;
    let embedding_blob: Option<Vec<u8>> = r.get(9)?;
    // RELIX-7.16 columns. The migration backfills these on
    // pre-7.16 databases; we still tolerate NULLs here as a
    // safety net (e.g. a row that lands via a future migration
    // path that hasn't run yet).
    let shareable: i32 = r.get(10).unwrap_or(0);
    let shared_with_json: Option<String> = r.get(11).unwrap_or(None);
    let shared_by: Option<String> = r.get(12).unwrap_or(None);
    let share_policy_s: Option<String> = r.get(13).unwrap_or(None);
    let source_trust_s: Option<String> = r.get(14).unwrap_or(None);
    let frozen: i32 = r.get(15).unwrap_or(0);
    let last_edited_ms: Option<i64> = r.get(16).unwrap_or(None);
    let consolidated: i32 = r.get(17).unwrap_or(0);
    // GAP 23: optional tenant_id column. NULL means "default
    // tenant" (single-tenant deployments).
    let tenant_id: Option<String> = r.get(18).unwrap_or(None);
    // GAP 18: bi-temporal supersedes pointer. NULL on the
    // current head of the chain; populated on retired rows.
    // `.unwrap_or(None)` because the migration column may not
    // exist on a future read path that selects fewer columns.
    let superseded_by: Option<String> = r.get(19).unwrap_or(None);
    Ok((|| {
        let layer = MemoryLayer::parse(&layer_s).ok_or_else(|| {
            LayeredMemoryError::Serialization(format!("unknown layer: {layer_s}"))
        })?;
        let tags: Vec<String> = serde_json::from_str(&tags_json)
            .map_err(|e| LayeredMemoryError::Serialization(e.to_string()))?;
        let embedding = embedding_blob.map(|b| decode_f32_le(&b));
        let shared_with: Vec<String> = match shared_with_json.as_deref() {
            None | Some("") => Vec::new(),
            Some(s) => serde_json::from_str(s).unwrap_or_default(),
        };
        let share_policy = share_policy_s
            .as_deref()
            .and_then(SharePolicy::parse)
            .unwrap_or(SharePolicy::None);
        let source_trust = source_trust_s
            .as_deref()
            .and_then(SourceTrust::parse)
            .unwrap_or(SourceTrust::Internal);
        Ok(MemoryRecord {
            id,
            layer,
            text,
            source,
            tags,
            created_at,
            valid_from,
            valid_to,
            observed_at,
            embedding,
            shareable: shareable != 0,
            shared_with,
            shared_by,
            share_policy,
            source_trust,
            frozen: frozen != 0,
            last_edited_ms,
            consolidated: consolidated != 0,
            tenant_id,
            superseded_by,
        })
    })())
}

fn encode_f32_le(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn decode_f32_le(b: &[u8]) -> Vec<f32> {
    let mut out = Vec::with_capacity(b.len() / 4);
    for chunk in b.chunks_exact(4) {
        let bytes: [u8; 4] = chunk.try_into().expect("chunks_exact(4) yields [u8; 4]");
        out.push(f32::from_le_bytes(bytes));
    }
    out
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Mint a stable u64 id from a record id string. Used by the
/// embedding pipeline when handing points to Qdrant — Qdrant's
/// integer point ids need a deterministic mapping from our
/// string ids so re-upserts hit the same row.
pub fn qdrant_point_id_from_str(id: &str) -> u64 {
    let hash = blake3::hash(id.as_bytes());
    let bytes = hash.as_bytes();
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, layer: MemoryLayer, text: &str, source: &str) -> MemoryRecord {
        let mut r = MemoryRecord::new_raw(id, text, source);
        r.layer = layer;
        r
    }

    #[test]
    fn layer_round_trips_via_string_form() {
        for l in [
            MemoryLayer::Raw,
            MemoryLayer::Semantic,
            MemoryLayer::Observation,
            MemoryLayer::Model,
        ] {
            assert_eq!(MemoryLayer::parse(l.as_str()), Some(l));
            assert_eq!(format!("{l}"), l.as_str());
        }
        assert!(MemoryLayer::parse("not-a-layer").is_none());
    }

    // ---- GAP 18: bi-temporal validity tests ---------------

    fn stamped_record(id: &str, text: &str, source: &str, ts: i64) -> MemoryRecord {
        let mut r = record(id, MemoryLayer::Observation, text, source);
        r.created_at = ts;
        r.valid_from = ts;
        r.observed_at = ts;
        r
    }

    #[test]
    fn supersede_marks_old_row_and_inserts_new_head_atomically() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        let old = stamped_record("a", "user lives in NY", "user.alice", 100);
        store.insert(&old).unwrap();
        let new = stamped_record("b", "user lives in SF", "user.alice", 200);
        store.supersede("a", &new, 200).unwrap();

        // a now points to b with valid_to = 200; b is head.
        let chain = store.supersedes_chain("a").unwrap();
        assert_eq!(chain, vec!["a".to_string(), "b".to_string()]);

        // The stored a row carries the supersedes pointer.
        let all = store.text_search("user lives", 10).unwrap();
        let a_row = all.iter().find(|r| r.id == "a").expect("a still present");
        assert_eq!(a_row.superseded_by.as_deref(), Some("b"));
        assert_eq!(a_row.valid_to, Some(200));
        let b_row = all.iter().find(|r| r.id == "b").expect("b inserted");
        assert!(b_row.superseded_by.is_none());
        assert!(b_row.valid_to.is_none());
    }

    #[test]
    fn supersede_errors_when_target_does_not_exist() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        let new = stamped_record("b", "x", "src", 200);
        let err = store
            .supersede("nope", &new, 200)
            .expect_err("missing target");
        assert!(format!("{err}").contains("nope"));
    }

    #[test]
    fn as_of_returns_only_records_valid_at_the_query_timestamp() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        let old = stamped_record("a", "fact1", "src", 100);
        store.insert(&old).unwrap();
        let new = stamped_record("b", "fact2", "src", 300);
        store.supersede("a", &new, 300).unwrap();

        // At t=150 only `a` was valid.
        let pre = store.as_of(150, "src", 10).unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0].id, "a");

        // At t=300, `a` has been retired (valid_to = 300 is
        // strict ">") and `b` is now head.
        let mid = store.as_of(300, "src", 10).unwrap();
        assert!(mid.iter().any(|r| r.id == "b"));
        assert!(!mid.iter().any(|r| r.id == "a"));

        // At t=999, only `b` is valid.
        let post = store.as_of(999, "src", 10).unwrap();
        assert_eq!(post.len(), 1);
        assert_eq!(post[0].id, "b");
    }

    #[test]
    fn as_of_with_empty_source_returns_every_row_in_the_window() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        store
            .insert(&stamped_record("a", "x", "src1", 100))
            .unwrap();
        store
            .insert(&stamped_record("b", "y", "src2", 110))
            .unwrap();
        let rows = store.as_of(200, "", 10).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn supersedes_chain_walks_multi_hop_history() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        store
            .insert(&stamped_record("a", "v1", "src", 100))
            .unwrap();
        store
            .supersede("a", &stamped_record("b", "v2", "src", 200), 200)
            .unwrap();
        store
            .supersede("b", &stamped_record("c", "v3", "src", 300), 300)
            .unwrap();
        store
            .supersede("c", &stamped_record("d", "v4", "src", 400), 400)
            .unwrap();
        let chain = store.supersedes_chain("a").unwrap();
        assert_eq!(
            chain,
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
            ]
        );
    }

    #[test]
    fn supersedes_chain_handles_uninvolved_head_record() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        store
            .insert(&stamped_record("solo", "alone", "src", 100))
            .unwrap();
        let chain = store.supersedes_chain("solo").unwrap();
        assert_eq!(chain, vec!["solo".to_string()]);
    }

    #[test]
    fn supersede_is_atomic_when_new_record_violates_index() {
        // Inserting the new record with the SAME id as an
        // unrelated existing row still succeeds because the
        // INSERT path uses OR REPLACE. The transaction
        // ensures the old row's valid_to is set even when the
        // new id collides with an existing one.
        let store = LayeredMemoryStore::in_memory().unwrap();
        store
            .insert(&stamped_record("a", "v1", "src", 100))
            .unwrap();
        store
            .insert(&stamped_record("c", "preexisting c", "src", 110))
            .unwrap();
        let new = stamped_record("c", "supersedes a", "src", 200);
        store.supersede("a", &new, 200).unwrap();
        // a got valid_to = 200 + superseded_by = c.
        let all = store.text_search("supersedes", 10).unwrap();
        assert!(all.iter().any(|r| r.id == "c"));
        let a_row = store
            .text_search("v1", 10)
            .unwrap()
            .into_iter()
            .find(|r| r.id == "a")
            .expect("a present");
        assert_eq!(a_row.superseded_by.as_deref(), Some("c"));
    }

    #[test]
    fn open_in_memory_creates_table() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        let conn = store.conn.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memory_records'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn file_backed_open_sets_wal_mode_and_creates_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mem.db");
        let store = LayeredMemoryStore::open(&path).unwrap();
        let conn = store.conn.lock().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        // Table is present.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='memory_records'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn insert_and_get_round_trip_a_record() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        let mut r = record(
            "rec-1",
            MemoryLayer::Raw,
            "the user prefers helvetica",
            "sess-abc",
        );
        r.tags = vec!["pref".into(), "font".into()];
        store.insert(&r).unwrap();
        let got = store.get("rec-1").unwrap().expect("must exist");
        assert_eq!(got.id, "rec-1");
        assert_eq!(got.layer, MemoryLayer::Raw);
        assert_eq!(got.text, "the user prefers helvetica");
        assert_eq!(got.tags, vec!["pref".to_string(), "font".to_string()]);
        assert!(got.embedding.is_none());
        assert!(store.get("missing").unwrap().is_none());
    }

    #[test]
    fn list_filters_by_layer_and_source() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        store
            .insert(&record("a", MemoryLayer::Raw, "one", "sess-1"))
            .unwrap();
        store
            .insert(&record("b", MemoryLayer::Raw, "two", "sess-2"))
            .unwrap();
        store
            .insert(&record("c", MemoryLayer::Observation, "three", "sess-1"))
            .unwrap();
        let raws = store.list(Some(MemoryLayer::Raw), None, 10, 0).unwrap();
        assert_eq!(raws.len(), 2);
        assert!(raws.iter().all(|r| r.layer == MemoryLayer::Raw));
        let s1 = store.list(None, Some("sess-1"), 10, 0).unwrap();
        assert_eq!(s1.len(), 2);
        let raws_s1 = store
            .list(Some(MemoryLayer::Raw), Some("sess-1"), 10, 0)
            .unwrap();
        assert_eq!(raws_s1.len(), 1);
        assert_eq!(raws_s1[0].id, "a");
    }

    #[test]
    fn text_search_returns_substring_matches() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        store
            .insert(&record("a", MemoryLayer::Raw, "deploy staging", "s"))
            .unwrap();
        store
            .insert(&record("b", MemoryLayer::Raw, "deploy production", "s"))
            .unwrap();
        store
            .insert(&record("c", MemoryLayer::Raw, "weather report", "s"))
            .unwrap();
        let hits = store.text_search("deploy", 10).unwrap();
        assert_eq!(hits.len(), 2);
        let none = store.text_search("noresults", 10).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn invalidate_sets_valid_to() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        store
            .insert(&record("a", MemoryLayer::Observation, "fact", "agent"))
            .unwrap();
        assert!(store.get("a").unwrap().unwrap().valid_to.is_none());
        store.invalidate("a", 12345).unwrap();
        let got = store.get("a").unwrap().unwrap();
        assert_eq!(got.valid_to, Some(12345));
    }

    #[test]
    fn update_embedding_persists_vector_blob() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        store
            .insert(&record("a", MemoryLayer::Raw, "hi", "s"))
            .unwrap();
        store.update_embedding("a", vec![0.5, -0.25, 1.0]).unwrap();
        let got = store.get("a").unwrap().unwrap();
        let v = got.embedding.expect("embedding present");
        assert_eq!(v.len(), 3);
        assert!((v[0] - 0.5).abs() < 1e-6);
        assert!((v[1] + 0.25).abs() < 1e-6);
        assert!((v[2] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn count_pending_embeddings_groups_by_layer() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        store
            .insert(&record("a", MemoryLayer::Raw, "one", "s"))
            .unwrap();
        store
            .insert(&record("b", MemoryLayer::Raw, "two", "s"))
            .unwrap();
        let mut c = record("c", MemoryLayer::Observation, "three", "s");
        c.embedding = Some(vec![1.0, 2.0, 3.0]);
        store.insert(&c).unwrap();
        let counts = store.count_pending_embeddings().unwrap();
        assert_eq!(counts.get(&MemoryLayer::Raw).copied().unwrap_or(0), 2);
        assert_eq!(
            counts.get(&MemoryLayer::Observation).copied().unwrap_or(0),
            0
        );
    }

    #[test]
    fn fetch_pending_returns_unembedded_records_in_age_order() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        let mut a = record("a", MemoryLayer::Raw, "first", "s");
        a.observed_at = 10;
        let mut b = record("b", MemoryLayer::Raw, "second", "s");
        b.observed_at = 20;
        let mut c = record("c", MemoryLayer::Raw, "embedded", "s");
        c.observed_at = 5;
        c.embedding = Some(vec![0.1; 4]);
        store.insert(&a).unwrap();
        store.insert(&b).unwrap();
        store.insert(&c).unwrap();
        let pending = store.fetch_pending_embeddings(10).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].id, "a");
        assert_eq!(pending[1].id, "b");
    }

    #[test]
    fn qdrant_point_id_is_deterministic() {
        let a1 = qdrant_point_id_from_str("rec-1");
        let a2 = qdrant_point_id_from_str("rec-1");
        let b = qdrant_point_id_from_str("rec-2");
        assert_eq!(a1, a2, "same input must yield the same id");
        assert_ne!(a1, b, "different inputs should land on different ids");
    }

    // ── RELIX-7.15 bulk-anonymize walker ───────────────────

    fn redact_anonymizer() -> crate::training::PiiAnonymizer {
        crate::training::PiiAnonymizer::from_config(&crate::training::PiiConfig {
            enabled: true,
            strategy: crate::training::PiiStrategy::Redact,
            overrides: Default::default(),
        })
    }

    #[test]
    fn bulk_anonymize_records_scrubs_every_layer_and_counts_changes() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        let raw = record("r", MemoryLayer::Raw, "email alice@example.com", "s1");
        let sem = record("s", MemoryLayer::Semantic, "phone is 555-123-4567", "s1");
        let obs = record(
            "o",
            MemoryLayer::Observation,
            "user lives at 1600 Pennsylvania Avenue",
            "s1",
        );
        let model = record("m", MemoryLayer::Model, "clean text only", "s1");
        store.insert(&raw).unwrap();
        store.insert(&sem).unwrap();
        store.insert(&obs).unwrap();
        store.insert(&model).unwrap();
        let stats = store.bulk_anonymize_records(&redact_anonymizer()).unwrap();
        assert_eq!(stats.raw.scanned, 1);
        assert_eq!(stats.raw.changed, 1);
        assert_eq!(stats.semantic.scanned, 1);
        assert_eq!(stats.semantic.changed, 1);
        assert_eq!(stats.observation.scanned, 1);
        assert_eq!(stats.observation.changed, 1);
        assert_eq!(stats.model.scanned, 1);
        // Layer 4 row has no PII — nothing to change.
        assert_eq!(stats.model.changed, 0);
        assert_eq!(stats.total_scanned(), 4);
        assert_eq!(stats.total_changed(), 3);
        // Spot-check the persisted text.
        let got_raw = store.get("r").unwrap().unwrap();
        assert!(got_raw.text.contains("[EMAIL]"));
        assert!(!got_raw.text.contains("alice@example.com"));
        let got_model = store.get("m").unwrap().unwrap();
        assert_eq!(got_model.text, "clean text only");
    }

    #[test]
    fn bulk_anonymize_records_is_idempotent_on_second_run() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        store
            .insert(&record(
                "x",
                MemoryLayer::Raw,
                "email alice@example.com",
                "s",
            ))
            .unwrap();
        let anon = redact_anonymizer();
        let first = store.bulk_anonymize_records(&anon).unwrap();
        assert_eq!(first.raw.changed, 1);
        let second = store.bulk_anonymize_records(&anon).unwrap();
        assert_eq!(
            second.raw.changed, 0,
            "second pass must be a no-op: {second:?}"
        );
        // But the row is still scanned.
        assert_eq!(second.raw.scanned, 1);
    }

    #[test]
    fn bulk_anonymize_records_with_disabled_anonymizer_changes_nothing() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        store
            .insert(&record("x", MemoryLayer::Raw, "alice@example.com", "s"))
            .unwrap();
        let disabled = crate::training::PiiAnonymizer::disabled();
        let stats = store.bulk_anonymize_records(&disabled).unwrap();
        assert_eq!(stats.raw.scanned, 1);
        assert_eq!(stats.raw.changed, 0);
        // Text is untouched.
        let r = store.get("x").unwrap().unwrap();
        assert!(r.text.contains("alice@example.com"));
    }

    // ── RELIX-7.16 sharing schema ──────────────────────────

    #[test]
    fn share_policy_round_trips_through_parse_and_as_str() {
        for p in [SharePolicy::None, SharePolicy::Explicit, SharePolicy::Auto] {
            assert_eq!(SharePolicy::parse(p.as_str()), Some(p));
        }
        // Empty / NULL-equivalent → None.
        assert_eq!(SharePolicy::parse(""), Some(SharePolicy::None));
        // Unknown values reject.
        assert!(SharePolicy::parse("publish").is_none());
    }

    #[test]
    fn new_raw_record_has_safe_share_defaults() {
        let r = MemoryRecord::new_raw("x", "hi", "s");
        assert!(!r.shareable);
        assert!(r.shared_with.is_empty());
        assert!(r.shared_by.is_none());
        assert_eq!(r.share_policy, SharePolicy::None);
    }

    #[test]
    fn insert_persists_share_fields_and_round_trips_through_get() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        let mut r = record(
            "share-a",
            MemoryLayer::Observation,
            "user prefers Helvetica",
            "alice",
        );
        r.shareable = true;
        r.shared_with = vec!["bob".into(), "carol".into()];
        r.shared_by = Some("alice".into());
        r.share_policy = SharePolicy::Auto;
        store.insert(&r).unwrap();
        let got = store.get("share-a").unwrap().unwrap();
        assert!(got.shareable);
        assert_eq!(
            got.shared_with,
            vec!["bob".to_string(), "carol".to_string()]
        );
        assert_eq!(got.shared_by.as_deref(), Some("alice"));
        assert_eq!(got.share_policy, SharePolicy::Auto);
    }

    #[test]
    fn pre_7_16_database_picks_up_share_columns_on_open() {
        use tempfile::TempDir;
        // Simulate a pre-7.16 database by opening a connection
        // directly + creating ONLY the legacy schema (no share
        // columns).
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy.db");
        {
            let conn = Connection::open(&path).unwrap();
            crate::db::apply_pragmas(&conn).unwrap();
            conn.execute_batch(
                "CREATE TABLE memory_records (\
                     id TEXT PRIMARY KEY,\
                     layer TEXT NOT NULL,\
                     text TEXT NOT NULL,\
                     source TEXT NOT NULL DEFAULT '',\
                     tags TEXT NOT NULL DEFAULT '[]',\
                     created_at INTEGER NOT NULL,\
                     valid_from INTEGER NOT NULL,\
                     valid_to INTEGER,\
                     observed_at INTEGER NOT NULL,\
                     embedding BLOB\
                 );\
                 INSERT INTO memory_records (id, layer, text, source, tags, created_at, valid_from, observed_at) \
                 VALUES ('legacy-1', 'observation', 'pre-7.16 row', 'alice', '[]', 100, 100, 100);",
            ).unwrap();
        }
        // Open via the production code path — this must run
        // the ALTER TABLE migration.
        let store = LayeredMemoryStore::open(&path).unwrap();
        let got = store.get("legacy-1").unwrap().unwrap();
        assert_eq!(got.text, "pre-7.16 row");
        assert!(!got.shareable, "default to non-shareable");
        assert!(got.shared_with.is_empty());
        assert!(got.shared_by.is_none());
        assert_eq!(got.share_policy, SharePolicy::None);
    }

    #[test]
    fn migration_is_idempotent_when_column_already_exists() {
        // Opening the in_memory store twice (well, opening then
        // re-running init_schema by closing + re-opening on a
        // file path) should not raise — column_exists must
        // short-circuit the ADD COLUMN.
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("idempotent.db");
        let _ = LayeredMemoryStore::open(&path).unwrap();
        // Drop the first handle; opening again triggers
        // init_schema a second time. Must not error.
        let store = LayeredMemoryStore::open(&path).unwrap();
        store
            .insert(&record(
                "post-migrate",
                MemoryLayer::Observation,
                "fact",
                "alice",
            ))
            .unwrap();
        let got = store.get("post-migrate").unwrap().unwrap();
        assert_eq!(got.share_policy, SharePolicy::None);
    }

    /// PART 4: helper that inserts a row pinned to a specific
    /// tenant_id so the tenant-aware read tests have data to
    /// look at.
    fn record_for_tenant(
        id: &str,
        layer: MemoryLayer,
        text: &str,
        source: &str,
        tenant: &str,
    ) -> MemoryRecord {
        let mut r = record(id, layer, text, source);
        r.tenant_id = Some(tenant.to_string());
        r
    }

    /// PART 4: tenant-aware text search filters by tenant_id
    /// and never returns rows from other tenants.
    #[test]
    fn fix_part4_text_search_for_tenant_isolates_buckets() {
        let store = LayeredMemoryStore::in_memory_with_tenant_isolation(true).unwrap();
        store
            .insert(&record_for_tenant(
                "a1",
                MemoryLayer::Raw,
                "shared secret",
                "alice",
                "acme",
            ))
            .unwrap();
        store
            .insert(&record_for_tenant(
                "b1",
                MemoryLayer::Raw,
                "shared secret",
                "bob",
                "globex",
            ))
            .unwrap();
        // acme sees only its own row.
        let acme_hits = store
            .text_search_for_tenant("shared", 10, Some("acme"))
            .unwrap();
        assert_eq!(acme_hits.len(), 1);
        assert_eq!(acme_hits[0].id, "a1");
        // globex sees only its own.
        let globex_hits = store
            .text_search_for_tenant("shared", 10, Some("globex"))
            .unwrap();
        assert_eq!(globex_hits.len(), 1);
        assert_eq!(globex_hits[0].id, "b1");
        // Unknown tenant sees nothing.
        let none_hits = store
            .text_search_for_tenant("shared", 10, Some("unknown"))
            .unwrap();
        assert!(none_hits.is_empty());
    }

    /// PART 4: tenant-aware read fails closed in
    /// tenant-isolation mode when the caller forgets to
    /// supply a tenant id.
    #[test]
    fn fix_part4_text_search_for_tenant_fails_closed_on_missing_tenant() {
        let store = LayeredMemoryStore::in_memory_with_tenant_isolation(true).unwrap();
        assert!(matches!(
            store.text_search_for_tenant("anything", 10, None),
            Err(LayeredMemoryError::MissingTenant)
        ));
        assert!(matches!(
            store.text_search_for_tenant("anything", 10, Some("")),
            Err(LayeredMemoryError::MissingTenant)
        ));
        assert!(matches!(
            store.text_search_for_tenant("anything", 10, Some("   ")),
            Err(LayeredMemoryError::MissingTenant)
        ));
    }

    /// PART 4: in legacy (isolation-off) mode the tenant-aware
    /// methods passthrough to the tenant-blind variants for
    /// backwards compatibility.
    #[test]
    fn fix_part4_text_search_for_tenant_passes_through_when_isolation_off() {
        let store = LayeredMemoryStore::in_memory().unwrap();
        assert!(!store.tenant_isolation_enabled());
        store
            .insert(&record_for_tenant(
                "x",
                MemoryLayer::Raw,
                "hi there",
                "alice",
                "acme",
            ))
            .unwrap();
        // In legacy mode passing None still returns the row.
        let hits = store.text_search_for_tenant("hi", 10, None).unwrap();
        assert_eq!(hits.len(), 1);
    }

    /// PART 4: tenant-aware `get` hides cross-tenant rows
    /// (Ok(None), not Err) so a leaked id can't reveal
    /// existence.
    #[test]
    fn fix_part4_get_for_tenant_hides_cross_tenant_rows() {
        let store = LayeredMemoryStore::in_memory_with_tenant_isolation(true).unwrap();
        store
            .insert(&record_for_tenant(
                "secret-id",
                MemoryLayer::Raw,
                "private",
                "alice",
                "acme",
            ))
            .unwrap();
        // Owner sees the row.
        let owner = store.get_for_tenant("secret-id", Some("acme")).unwrap();
        assert!(owner.is_some());
        // Other tenant gets Ok(None) — same shape as a real
        // not-found, so cross-tenant existence is invisible.
        let other = store.get_for_tenant("secret-id", Some("globex")).unwrap();
        assert!(other.is_none());
        // Missing tenant id is an error, not a silent miss.
        assert!(matches!(
            store.get_for_tenant("secret-id", None),
            Err(LayeredMemoryError::MissingTenant)
        ));
    }
}
