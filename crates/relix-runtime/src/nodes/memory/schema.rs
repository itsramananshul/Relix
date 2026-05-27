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

/// One row in the four-layer memory store. `id` is the stable
/// identifier — the embedding pipeline + Qdrant payload both
/// reference it directly so updates are idempotent.
#[derive(Clone, Debug, PartialEq)]
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
        }
    }
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
}

/// SQLite-backed store for the four-layer schema. The
/// connection is wrapped in `Arc<Mutex<_>>` because rusqlite's
/// `Connection` is not `Sync`; the memory node's handlers run
/// concurrently.
#[derive(Clone)]
pub struct LayeredMemoryStore {
    conn: Arc<Mutex<Connection>>,
}

impl LayeredMemoryStore {
    /// Open the store at `path`. Creates the parent directory
    /// and the table if absent; applies the project-wide
    /// SQLite pragmas (WAL, foreign_keys, busy timeout).
    pub fn open(path: &Path) -> Result<Self, LayeredMemoryError> {
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
        })
    }

    /// In-memory store for tests. Skips the WAL pragma (no-op
    /// on `:memory:` anyway) but keeps foreign_keys + busy
    /// timeout consistent with the file-backed open path.
    pub fn in_memory() -> Result<Self, LayeredMemoryError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Insert a new record. Idempotent on `id` collision via
    /// `INSERT OR REPLACE` so the caller can re-insert a
    /// record without checking existence first (the memory
    /// pipeline relies on this when re-processing a batch).
    pub fn insert(&self, record: &MemoryRecord) -> Result<(), LayeredMemoryError> {
        let conn = self.conn.lock().map_err(|_| LayeredMemoryError::Lock)?;
        let tags_json = serde_json::to_string(&record.tags)
            .map_err(|e| LayeredMemoryError::Serialization(e.to_string()))?;
        let embedding_blob = record.embedding.as_ref().map(|v| encode_f32_le(v));
        conn.execute(
            "INSERT OR REPLACE INTO memory_records \
             (id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding \
                 FROM memory_records WHERE id = ?1",
                params![id],
                row_to_record,
            )
            .optional()?;
        row.transpose()
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
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding \
                 FROM memory_records \
                 ORDER BY created_at DESC, id ASC \
                 LIMIT ?1 OFFSET ?2",
                vec![limit.into(), offset.into()],
            ),
            (Some(l), None) => (
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding \
                 FROM memory_records WHERE layer = ?3 \
                 ORDER BY created_at DESC, id ASC \
                 LIMIT ?1 OFFSET ?2",
                vec![limit.into(), offset.into(), l.as_str().to_string().into()],
            ),
            (None, Some(s)) => (
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding \
                 FROM memory_records WHERE source = ?3 \
                 ORDER BY created_at DESC, id ASC \
                 LIMIT ?1 OFFSET ?2",
                vec![limit.into(), offset.into(), s.to_string().into()],
            ),
            (Some(l), Some(s)) => (
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding \
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
            "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding \
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
                "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding \
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
            "SELECT id, layer, text, source, tags, created_at, valid_from, valid_to, observed_at, embedding \
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
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_records (\
             id          TEXT PRIMARY KEY,\
             layer       TEXT NOT NULL,\
             text        TEXT NOT NULL,\
             source      TEXT NOT NULL DEFAULT '',\
             tags        TEXT NOT NULL DEFAULT '[]',\
             created_at  INTEGER NOT NULL,\
             valid_from  INTEGER NOT NULL,\
             valid_to    INTEGER,\
             observed_at INTEGER NOT NULL,\
             embedding   BLOB\
         );\
         CREATE INDEX IF NOT EXISTS memory_records_layer_created \
             ON memory_records(layer, created_at DESC);\
         CREATE INDEX IF NOT EXISTS memory_records_source \
             ON memory_records(source);\
         CREATE INDEX IF NOT EXISTS memory_records_pending \
             ON memory_records(observed_at) WHERE embedding IS NULL;",
    )?;
    Ok(())
}

/// `SELECT` row → `Result<MemoryRecord, LayeredMemoryError>`,
/// folded through rusqlite's `Result` so we can use it inside
/// `query_map`. The outer `Result<_>` is rusqlite's; the inner
/// `Result<_>` is ours (returns the parse error on bad rows).
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
    Ok((|| {
        let layer = MemoryLayer::parse(&layer_s).ok_or_else(|| {
            LayeredMemoryError::Serialization(format!("unknown layer: {layer_s}"))
        })?;
        let tags: Vec<String> = serde_json::from_str(&tags_json)
            .map_err(|e| LayeredMemoryError::Serialization(e.to_string()))?;
        let embedding = embedding_blob.map(|b| decode_f32_le(&b));
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
}
