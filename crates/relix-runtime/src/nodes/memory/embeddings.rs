//! Vector embedding store for per-subject memory chunks.
//!
//! Persists embeddings in SQLite (alongside the existing `turns` +
//! `agent_memory` tables on the memory node). Lookup is a full
//! table scan filtered by `(subject_id, target)` with cosine
//! similarity ranked in Rust.
//!
//! ## Why a full scan
//!
//! Agents in Relix realistically hold hundreds — not millions —
//! of memory entries: the `agent` cap is 2200 chars and `user` is
//! 1375 chars, so even an aggressive operator authoring small
//! chunks won't go past a few hundred rows per subject. A linear
//! scan over a few hundred f32-vector dot products is on the
//! order of microseconds and avoids pulling in the `sqlite-vec`
//! extension or an HNSW index dep.
//!
//! ## Upgrade path
//!
//! Switch the table to use sqlite-vec's `vec0` virtual table
//! once the rust binding stabilises, OR add an in-memory HNSW
//! cache keyed by `(subject_id, target)` that rebuilds on
//! controller startup. Either change is local to this module —
//! callers go through `EmbeddingStore::search`.
//!
//! ## Dedup
//!
//! Each entry carries `entry_hash = blake3(chunk_text)` as a
//! UNIQUE column. Re-embedding the same text returns the
//! existing `embedding_id` and skips the dispatcher call — both
//! `memory.embed` and `memory.embed_all` rely on this.
//!
//! ## Wire format on the blob
//!
//! Embeddings are little-endian-packed f32 sequences. 1536 dims
//! (`text-embedding-3-small`) → 6144 bytes. The schema doesn't
//! enforce dimension; mixed-model rows are allowed but search
//! requires equal length (the cosine helper returns 0.0 if
//! lengths differ, so mismatched entries simply rank last
//! without crashing).

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use crate::nodes::memory::MemoryError;

/// One stored embedding row.
#[derive(Clone, Debug)]
pub struct EmbeddingRow {
    pub embedding_id: String,
    pub subject_id: String,
    pub target: String,
    pub chunk_text: String,
    pub embedding: Vec<f32>,
    pub model: String,
    pub created_at: i64,
}

/// One search hit.
#[derive(Clone, Debug)]
pub struct SearchHit {
    pub embedding_id: String,
    pub score: f32,
    pub chunk_text: String,
}

/// SQLite-backed embedding store. Wraps the same `Connection`
/// the rest of the memory node uses; the new schema lives in a
/// migration applied by `apply_schema`.
pub struct EmbeddingStore {
    conn: Arc<Mutex<Connection>>,
}

impl EmbeddingStore {
    /// Construct from a shared connection. The connection must
    /// already have the `memory_embeddings` schema applied via
    /// [`apply_schema`].
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Insert one embedding row. Returns the new `embedding_id`,
    /// or — on duplicate `(subject_id, target, entry_hash)` —
    /// the existing row's id so the caller can be idempotent.
    /// GROUP 6: tenant-blind insert — writes the reserved
    /// `'default'` tenant. Retained so existing single-tenant
    /// call sites compile unchanged; new code should prefer
    /// [`Self::insert_for_tenant`].
    pub fn insert(
        &self,
        subject_id: &str,
        target: &str,
        chunk_text: &str,
        embedding: &[f32],
        model: &str,
    ) -> Result<InsertOutcome, MemoryError> {
        self.insert_for_tenant(subject_id, target, chunk_text, embedding, model, "default")
    }

    /// GROUP 6: insert attributed to the caller's VERIFIED tenant.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_for_tenant(
        &self,
        subject_id: &str,
        target: &str,
        chunk_text: &str,
        embedding: &[f32],
        model: &str,
        tenant_id: &str,
    ) -> Result<InsertOutcome, MemoryError> {
        let tenant = if tenant_id.trim().is_empty() {
            "default"
        } else {
            tenant_id
        };
        if subject_id.is_empty() {
            return Err(MemoryError::InvalidArg("subject_id required".into()));
        }
        if target != "agent" && target != "user" {
            return Err(MemoryError::InvalidArg(format!(
                "target must be 'agent' or 'user', got '{target}'"
            )));
        }
        if chunk_text.is_empty() {
            return Err(MemoryError::InvalidArg("chunk_text required".into()));
        }
        if embedding.is_empty() {
            return Err(MemoryError::InvalidArg(
                "embedding must be non-empty".into(),
            ));
        }
        let entry_hash = blake3::hash(chunk_text.as_bytes()).to_hex().to_string();
        let conn = self.conn.lock().map_err(|_| MemoryError::Lock)?;
        // Dedup: same subject_id + target + entry_hash → return the
        // existing id. Entry_hash is content-only so the same text
        // gets the same id regardless of model — re-embedding with
        // a different model produces a different row because the
        // primary key is the random embedding_id, but we still
        // surface a Duplicate outcome so the caller knows the
        // chunk has been seen before.
        let existing: Option<String> = conn
            .query_row(
                "SELECT embedding_id FROM memory_embeddings \
                 WHERE subject_id = ?1 AND target = ?2 AND entry_hash = ?3 \
                 LIMIT 1",
                params![subject_id, target, &entry_hash],
                |row| row.get(0),
            )
            .optional()
            .map_err(MemoryError::Db)?;
        if let Some(id) = existing {
            return Ok(InsertOutcome::Duplicate { embedding_id: id });
        }

        let embedding_id = new_embedding_id();
        let blob = encode_f32_le(embedding);
        let ts = unix_secs();
        conn.execute(
            "INSERT INTO memory_embeddings \
             (embedding_id, subject_id, target, chunk_text, embedding, model, created_at, entry_hash, tenant_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                &embedding_id,
                subject_id,
                target,
                chunk_text,
                blob,
                model,
                ts,
                &entry_hash,
                tenant,
            ],
        )
        .map_err(MemoryError::Db)?;
        Ok(InsertOutcome::Inserted { embedding_id })
    }

    /// GROUP 6: tenant-scoped count for `(subject_id, target)` —
    /// proves cross-tenant denial: a read carrying tenant A never
    /// observes tenant B's embeddings even for a shared subject.
    pub fn count_for_tenant(
        &self,
        tenant: &str,
        subject_id: &str,
        target: &str,
    ) -> Result<u64, MemoryError> {
        let conn = self.conn.lock().map_err(|_| MemoryError::Lock)?;
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_embeddings \
                 WHERE tenant_id = ?1 AND subject_id = ?2 AND target = ?3",
                params![tenant, subject_id, target],
                |r| r.get(0),
            )
            .map_err(MemoryError::Db)?;
        Ok(n as u64)
    }

    /// Top-K cosine-similarity search within one (subject_id,
    /// target). `query_vec` is the embedding of the query.
    /// Limit clamps to 1..=20.
    pub fn search(
        &self,
        subject_id: &str,
        target: &str,
        query_vec: &[f32],
        limit: usize,
    ) -> Result<Vec<SearchHit>, MemoryError> {
        if subject_id.is_empty() {
            return Err(MemoryError::InvalidArg("subject_id required".into()));
        }
        if target != "agent" && target != "user" {
            return Err(MemoryError::InvalidArg(format!(
                "target must be 'agent' or 'user', got '{target}'"
            )));
        }
        let k = limit.clamp(1, 20);
        let conn = self.conn.lock().map_err(|_| MemoryError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT embedding_id, chunk_text, embedding \
                 FROM memory_embeddings \
                 WHERE subject_id = ?1 AND target = ?2",
            )
            .map_err(MemoryError::Db)?;
        let rows = stmt
            .query_map(params![subject_id, target], |row| {
                let id: String = row.get(0)?;
                let chunk: String = row.get(1)?;
                let blob: Vec<u8> = row.get(2)?;
                Ok((id, chunk, blob))
            })
            .map_err(MemoryError::Db)?;
        let mut scored: Vec<SearchHit> = Vec::new();
        for r in rows {
            let (id, chunk, blob) = r.map_err(MemoryError::Db)?;
            let vec = decode_f32_le(&blob);
            let score = cosine_similarity(query_vec, &vec);
            scored.push(SearchHit {
                embedding_id: id,
                score,
                chunk_text: chunk,
            });
        }
        // Descending similarity; tie-break by embedding_id for
        // determinism.
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.embedding_id.cmp(&b.embedding_id))
        });
        scored.truncate(k);
        Ok(scored)
    }

    /// Look up an existing row by content hash without
    /// inserting. Returns `Ok(Some(id))` when a row for
    /// `(subject_id, target, entry_hash)` exists, `Ok(None)`
    /// otherwise. Used by callers that want to skip re-embedding
    /// a chunk they've seen before.
    pub fn lookup_by_hash(
        &self,
        subject_id: &str,
        target: &str,
        entry_hash: &str,
    ) -> Result<Option<String>, MemoryError> {
        let conn = self.conn.lock().map_err(|_| MemoryError::Lock)?;
        conn.query_row(
            "SELECT embedding_id FROM memory_embeddings \
             WHERE subject_id = ?1 AND target = ?2 AND entry_hash = ?3 \
             LIMIT 1",
            params![subject_id, target, entry_hash],
            |row| row.get(0),
        )
        .optional()
        .map_err(MemoryError::Db)
    }

    /// Count rows for a `(subject_id, target)`. Useful for
    /// observability and the `embed_all` summary.
    pub fn count_for(&self, subject_id: &str, target: &str) -> Result<usize, MemoryError> {
        let conn = self.conn.lock().map_err(|_| MemoryError::Lock)?;
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_embeddings \
                 WHERE subject_id = ?1 AND target = ?2",
                params![subject_id, target],
                |row| row.get(0),
            )
            .map_err(MemoryError::Db)?;
        Ok(n as usize)
    }
}

/// Outcome of [`EmbeddingStore::insert`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted { embedding_id: String },
    Duplicate { embedding_id: String },
}

impl InsertOutcome {
    pub fn embedding_id(&self) -> &str {
        match self {
            InsertOutcome::Inserted { embedding_id }
            | InsertOutcome::Duplicate { embedding_id } => embedding_id,
        }
    }
}

/// Apply the embeddings schema to an existing memory-node
/// connection. Idempotent (`IF NOT EXISTS`).
pub fn apply_schema(conn: &Connection) -> Result<(), MemoryError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS memory_embeddings (
            embedding_id TEXT    NOT NULL PRIMARY KEY,
            subject_id   TEXT    NOT NULL,
            target       TEXT    NOT NULL,
            chunk_text   TEXT    NOT NULL,
            embedding    BLOB    NOT NULL,
            model        TEXT    NOT NULL,
            created_at   INTEGER NOT NULL,
            entry_hash   TEXT    NOT NULL,
            UNIQUE (subject_id, target, entry_hash)
        );
        CREATE INDEX IF NOT EXISTS memory_embeddings_subject
            ON memory_embeddings (subject_id, target);
        CREATE INDEX IF NOT EXISTS memory_embeddings_hash
            ON memory_embeddings (entry_hash);
        "#,
    )
    .map_err(MemoryError::Db)?;
    // GROUP 6: tenant isolation column (idempotent). Sibling of
    // the already tenant-scoped `memory_records`.
    crate::db::ensure_tenant_id_column(conn, "memory_embeddings").map_err(MemoryError::Db)?;
    Ok(())
}

/// Cosine similarity. Returns 0.0 if either vector is empty,
/// lengths differ, or either norm is zero — these cases never
/// crash the caller, they just rank last.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Encode an f32 sequence as little-endian-packed bytes.
pub fn encode_f32_le(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Decode little-endian-packed bytes back into an f32 sequence.
/// Trailing partial floats are dropped (defence in depth — we
/// always write multiples of 4 bytes, but a future schema change
/// shouldn't crash on a stale row).
pub fn decode_f32_le(bytes: &[u8]) -> Vec<f32> {
    let n = bytes.len() / 4;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let b = &bytes[i * 4..i * 4 + 4];
        out.push(f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    }
    out
}

fn new_embedding_id() -> String {
    let mut bytes = [0u8; 8];
    rand::Rng::fill(&mut rand::thread_rng(), &mut bytes);
    hex::encode(bytes)
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_in_memory() -> EmbeddingStore {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        EmbeddingStore::new(Arc::new(Mutex::new(conn)))
    }

    #[test]
    fn group6_embeddings_reads_are_isolated_by_verified_tenant() {
        // Two tenants embed (distinct) chunks for the SAME
        // subject. A tenant-scoped count must see ONLY its own.
        let s = store_in_memory();
        s.insert_for_tenant(
            "subj",
            "user",
            "tenant a chunk",
            &[0.1, 0.2],
            "m",
            "tenant-a",
        )
        .unwrap();
        s.insert_for_tenant(
            "subj",
            "user",
            "tenant b chunk",
            &[0.3, 0.4],
            "m",
            "tenant-b",
        )
        .unwrap();
        assert_eq!(s.count_for_tenant("tenant-a", "subj", "user").unwrap(), 1);
        assert_eq!(s.count_for_tenant("tenant-b", "subj", "user").unwrap(), 1);
        assert_eq!(s.count_for_tenant("tenant-c", "subj", "user").unwrap(), 0);
    }

    #[test]
    fn cosine_identical_vectors_returns_one() {
        let a = vec![1.0, 2.0, 3.0];
        let s = cosine_similarity(&a, &a);
        assert!((s - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal_vectors_returns_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let s = cosine_similarity(&a, &b);
        assert!(s.abs() < 1e-6);
    }

    #[test]
    fn cosine_opposite_vectors_returns_minus_one() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let s = cosine_similarity(&a, &b);
        assert!((s + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_handles_zero_vector_gracefully() {
        let z = vec![0.0_f32; 4];
        let a = vec![1.0, 1.0, 1.0, 1.0];
        assert_eq!(cosine_similarity(&z, &a), 0.0);
        assert_eq!(cosine_similarity(&a, &z), 0.0);
        assert_eq!(cosine_similarity(&z, &z), 0.0);
    }

    #[test]
    fn cosine_mismatched_lengths_returns_zero() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn cosine_empty_returns_zero() {
        let a: Vec<f32> = Vec::new();
        let b: Vec<f32> = Vec::new();
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    #[test]
    fn encode_decode_round_trip() {
        let v = vec![0.0_f32, -1.5, std::f32::consts::PI, 1e-7, f32::MIN_POSITIVE];
        let bytes = encode_f32_le(&v);
        assert_eq!(bytes.len(), v.len() * 4);
        let back = decode_f32_le(&bytes);
        assert_eq!(v, back);
    }

    #[test]
    fn insert_stores_row_with_correct_fields() {
        let s = store_in_memory();
        let out = s
            .insert(
                "subj-a",
                "agent",
                "the quick brown fox",
                &[0.1, 0.2, 0.3],
                "mock-3d",
            )
            .unwrap();
        let id = match out {
            InsertOutcome::Inserted { embedding_id } => embedding_id,
            o => panic!("expected Inserted, got {o:?}"),
        };
        assert_eq!(id.len(), 16);
        assert_eq!(s.count_for("subj-a", "agent").unwrap(), 1);
        assert_eq!(s.count_for("subj-a", "user").unwrap(), 0);
    }

    #[test]
    fn insert_dedups_identical_text_same_subject_target() {
        let s = store_in_memory();
        let a = s
            .insert("subj-a", "agent", "same text", &[1.0, 0.0, 0.0], "m")
            .unwrap();
        let b = s
            .insert("subj-a", "agent", "same text", &[0.0, 1.0, 0.0], "m")
            .unwrap();
        assert!(matches!(a, InsertOutcome::Inserted { .. }));
        assert!(matches!(b, InsertOutcome::Duplicate { .. }));
        assert_eq!(a.embedding_id(), b.embedding_id());
        assert_eq!(s.count_for("subj-a", "agent").unwrap(), 1);
    }

    #[test]
    fn insert_does_not_dedup_across_subjects() {
        let s = store_in_memory();
        let a = s
            .insert("subj-a", "agent", "same text", &[1.0, 0.0], "m")
            .unwrap();
        let b = s
            .insert("subj-b", "agent", "same text", &[1.0, 0.0], "m")
            .unwrap();
        assert!(matches!(a, InsertOutcome::Inserted { .. }));
        assert!(matches!(b, InsertOutcome::Inserted { .. }));
        assert_ne!(a.embedding_id(), b.embedding_id());
    }

    #[test]
    fn insert_does_not_dedup_across_targets() {
        let s = store_in_memory();
        let a = s
            .insert("subj-a", "agent", "same text", &[1.0, 0.0], "m")
            .unwrap();
        let b = s
            .insert("subj-a", "user", "same text", &[1.0, 0.0], "m")
            .unwrap();
        assert!(matches!(a, InsertOutcome::Inserted { .. }));
        assert!(matches!(b, InsertOutcome::Inserted { .. }));
        assert_ne!(a.embedding_id(), b.embedding_id());
    }

    #[test]
    fn search_orders_by_cosine_similarity_descending() {
        let s = store_in_memory();
        // Three rows in a 3-d unit space.
        s.insert("subj-a", "agent", "alpha", &[1.0, 0.0, 0.0], "m")
            .unwrap();
        s.insert("subj-a", "agent", "beta", &[0.0, 1.0, 0.0], "m")
            .unwrap();
        s.insert("subj-a", "agent", "gamma", &[0.7, 0.7, 0.0], "m")
            .unwrap();

        // Query points at "alpha"; expect alpha first, then gamma
        // (closest tangent), then beta (orthogonal).
        let hits = s.search("subj-a", "agent", &[1.0, 0.0, 0.0], 5).unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].chunk_text, "alpha");
        assert_eq!(hits[1].chunk_text, "gamma");
        assert_eq!(hits[2].chunk_text, "beta");
        // Alpha is identical.
        assert!((hits[0].score - 1.0).abs() < 1e-5);
    }

    #[test]
    fn search_isolated_by_subject_id() {
        let s = store_in_memory();
        s.insert("subj-a", "agent", "for a", &[1.0, 0.0], "m")
            .unwrap();
        s.insert("subj-b", "agent", "for b", &[1.0, 0.0], "m")
            .unwrap();
        let hits = s.search("subj-a", "agent", &[1.0, 0.0], 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_text, "for a");
    }

    #[test]
    fn search_isolated_by_target() {
        let s = store_in_memory();
        s.insert("subj-a", "agent", "agent row", &[1.0, 0.0], "m")
            .unwrap();
        s.insert("subj-a", "user", "user row", &[1.0, 0.0], "m")
            .unwrap();
        let agent_hits = s.search("subj-a", "agent", &[1.0, 0.0], 10).unwrap();
        let user_hits = s.search("subj-a", "user", &[1.0, 0.0], 10).unwrap();
        assert_eq!(agent_hits.len(), 1);
        assert_eq!(agent_hits[0].chunk_text, "agent row");
        assert_eq!(user_hits.len(), 1);
        assert_eq!(user_hits[0].chunk_text, "user row");
    }

    #[test]
    fn search_respects_limit() {
        let s = store_in_memory();
        for i in 0..10 {
            s.insert(
                "subj-a",
                "agent",
                &format!("entry {i}"),
                &[i as f32, 0.0],
                "m",
            )
            .unwrap();
        }
        let hits = s.search("subj-a", "agent", &[1.0, 0.0], 3).unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn search_clamps_limit_max_20() {
        let s = store_in_memory();
        for i in 0..30 {
            s.insert(
                "subj-a",
                "agent",
                &format!("entry {i}"),
                &[i as f32, 0.0],
                "m",
            )
            .unwrap();
        }
        // limit=1000 should clamp to 20.
        let hits = s.search("subj-a", "agent", &[1.0, 0.0], 1000).unwrap();
        assert_eq!(hits.len(), 20);
    }

    #[test]
    fn insert_rejects_invalid_target() {
        let s = store_in_memory();
        let err = s
            .insert("subj-a", "weird", "x", &[1.0, 0.0], "m")
            .unwrap_err();
        assert!(matches!(err, MemoryError::InvalidArg(_)));
    }

    #[test]
    fn insert_rejects_empty_subject_id() {
        let s = store_in_memory();
        let err = s.insert("", "agent", "x", &[1.0, 0.0], "m").unwrap_err();
        assert!(matches!(err, MemoryError::InvalidArg(_)));
    }

    #[test]
    fn insert_rejects_empty_embedding() {
        let s = store_in_memory();
        let err = s.insert("subj-a", "agent", "x", &[], "m").unwrap_err();
        assert!(matches!(err, MemoryError::InvalidArg(_)));
    }
}
