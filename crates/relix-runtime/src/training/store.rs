//! RELIX-7.15 — SQLite store for the training data pipeline.
//!
//! ```text
//! CREATE TABLE training_interactions (
//!   interaction_id    TEXT PRIMARY KEY,
//!   session_id        TEXT NOT NULL,
//!   agent             TEXT NOT NULL,
//!   model             TEXT NOT NULL,
//!   provider          TEXT NOT NULL,
//!   system_prompt     TEXT NOT NULL,
//!   user_message      TEXT NOT NULL,
//!   response          TEXT NOT NULL,
//!   tool_calls_json   TEXT NOT NULL,
//!   token_count       INTEGER,
//!   prompt_tokens     INTEGER,
//!   completion_tokens INTEGER,
//!   latency_ms        INTEGER NOT NULL,
//!   success           INTEGER NOT NULL,
//!   error_kind        TEXT,
//!   recorded_at       INTEGER NOT NULL,
//!   quality_score     REAL,
//!   exported          INTEGER NOT NULL DEFAULT 0,
//!   export_set        TEXT
//! );
//! CREATE INDEX training_interactions_session ON training_interactions(session_id);
//! CREATE INDEX training_interactions_agent   ON training_interactions(agent);
//! CREATE INDEX training_interactions_ts      ON training_interactions(recorded_at DESC);
//! CREATE INDEX training_interactions_score   ON training_interactions(quality_score);
//! CREATE INDEX training_interactions_export  ON training_interactions(exported, quality_score DESC);
//! ```
//!
//! Inserts are append-only; the only post-insert mutations are
//! `quality_score` (set by the scorer) and `exported` +
//! `export_set` (set by the export engine). Deletions are
//! operator-driven via `training.delete_interaction`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use super::types::{
    GroupedCount, InteractionId, InteractionRecord, InteractionSummary, ScoreDistribution,
    ToolCallRecord, TrainingStats,
};

#[derive(Debug, thiserror::Error)]
pub enum TrainingStoreError {
    #[error("training store io: {0}")]
    Io(String),
    #[error("training store sqlite: {0}")]
    Db(String),
    #[error("training store encode: {0}")]
    Encode(String),
    #[error("training store lock poisoned")]
    Lock,
}

impl From<rusqlite::Error> for TrainingStoreError {
    fn from(e: rusqlite::Error) -> Self {
        TrainingStoreError::Db(e.to_string())
    }
}

/// Filters consumed by both `list_interactions` and the export
/// engine. All fields are optional except the booleans, which
/// have sensible defaults at the caller level.
#[derive(Clone, Debug, Default)]
pub struct ListFilters {
    pub agent: Option<String>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub min_quality_score: Option<f32>,
    /// Optional inclusive lower bound on `recorded_at`, in unix
    /// milliseconds. `None` means "no lower bound".
    pub date_from: Option<i64>,
    /// Optional inclusive upper bound on `recorded_at`, in unix
    /// milliseconds. `None` means "no upper bound".
    pub date_to: Option<i64>,
    /// When `Some(true)` only exported rows match; when
    /// `Some(false)` only un-exported rows match; `None` means
    /// "either".
    pub exported: Option<bool>,
    /// When `true`, only rows with a non-null
    /// `quality_score` are returned. Used by the export engine's
    /// `min_quality_score = 0.0` edge case.
    pub require_scored: bool,
}

#[derive(Clone)]
pub struct TrainingStore {
    conn: Arc<Mutex<Connection>>,
}

impl TrainingStore {
    pub fn open(path: &Path) -> Result<Self, TrainingStoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| TrainingStoreError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn in_memory() -> Result<Self, TrainingStoreError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn insert(&self, rec: &InteractionRecord) -> Result<(), TrainingStoreError> {
        self.insert_for_tenant(rec, "default")
    }

    /// GROUP 6: insert attributed to the caller's VERIFIED tenant.
    pub fn insert_for_tenant(
        &self,
        rec: &InteractionRecord,
        tenant_id: &str,
    ) -> Result<(), TrainingStoreError> {
        let conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        insert_one(&conn, rec, tenant_id)
    }

    pub fn insert_batch(&self, batch: &[InteractionRecord]) -> Result<(), TrainingStoreError> {
        self.insert_batch_for_tenant(batch, "default")
    }

    /// GROUP 6: batch insert attributed to a single VERIFIED tenant.
    pub fn insert_batch_for_tenant(
        &self,
        batch: &[InteractionRecord],
        tenant_id: &str,
    ) -> Result<(), TrainingStoreError> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        let tx = conn.transaction()?;
        for rec in batch {
            insert_one(&tx, rec, tenant_id)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// GROUP 6: tenant-scoped lookup. Returns the interaction ONLY
    /// when it belongs to `tenant` — a caller scoped to tenant A
    /// asking for tenant B's `interaction_id` (or session_id) gets
    /// `None`. The `tenant_id = ?` predicate is enforced in SQL.
    pub fn count_for_tenant_and_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<u64, TrainingStoreError> {
        let conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM training_interactions WHERE tenant_id = ?1 AND session_id = ?2",
            params![tenant, session_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    pub fn get(
        &self,
        interaction_id: &str,
    ) -> Result<Option<InteractionRecord>, TrainingStoreError> {
        let conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT interaction_id, session_id, agent, model, provider, \
                    system_prompt, user_message, response, tool_calls_json, \
                    token_count, prompt_tokens, completion_tokens, latency_ms, \
                    success, error_kind, recorded_at, quality_score, exported, export_set, \
                    anonymized \
             FROM training_interactions WHERE interaction_id = ?1",
        )?;
        let row = stmt
            .query_row(params![interaction_id], row_to_record)
            .optional()?;
        Ok(row)
    }

    /// Delete one interaction. Returns `true` when a row was
    /// removed.
    pub fn delete(&self, interaction_id: &str) -> Result<bool, TrainingStoreError> {
        let conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        let n = conn.execute(
            "DELETE FROM training_interactions WHERE interaction_id = ?1",
            params![interaction_id],
        )?;
        Ok(n > 0)
    }

    /// Set / clear the `quality_score` column for one
    /// interaction. Returns `true` when a row was updated.
    pub fn set_quality_score(
        &self,
        interaction_id: &str,
        score: Option<f32>,
    ) -> Result<bool, TrainingStoreError> {
        let conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        let n = conn.execute(
            "UPDATE training_interactions SET quality_score = ?1 WHERE interaction_id = ?2",
            params![score.map(|s| s as f64), interaction_id],
        )?;
        Ok(n > 0)
    }

    /// Replace `system_prompt` / `user_message` / `response` /
    /// `tool_calls_json` and flip `anonymized = 1` for one
    /// interaction. Used by the export engine's safety-net
    /// path: rows recorded before anonymization was enabled
    /// get anonymized on first export, then the on-disk row is
    /// rewritten so the redaction is permanent.
    pub fn store_anonymized_content(
        &self,
        interaction_id: &str,
        system_prompt: &str,
        user_message: &str,
        response: &str,
        tool_calls: &[ToolCallRecord],
    ) -> Result<bool, TrainingStoreError> {
        let tool_calls_json = serde_json::to_string(tool_calls)
            .map_err(|e| TrainingStoreError::Encode(e.to_string()))?;
        let conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        let n = conn.execute(
            "UPDATE training_interactions \
             SET system_prompt = ?1, user_message = ?2, response = ?3, \
                 tool_calls_json = ?4, anonymized = 1 \
             WHERE interaction_id = ?5",
            params![
                system_prompt,
                user_message,
                response,
                tool_calls_json,
                interaction_id,
            ],
        )?;
        Ok(n > 0)
    }

    /// CORR PART 5: record the intended export-file path for
    /// a (`export_set`, `ids_hash`) pair BEFORE the file is
    /// written. Idempotent — re-inserting the same key is a
    /// no-op (ON CONFLICT DO NOTHING).
    pub fn stage_export_path(
        &self,
        export_set: &str,
        ids_hash: &str,
        path: &str,
    ) -> Result<(), TrainingStoreError> {
        let conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS training_export_passes (\
                 export_set TEXT NOT NULL, \
                 ids_hash   TEXT NOT NULL, \
                 file_path  TEXT NOT NULL, \
                 staged_at  INTEGER NOT NULL, \
                 PRIMARY KEY (export_set, ids_hash) \
             )",
            [],
        )?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
            .unwrap_or(0);
        conn.execute(
            "INSERT INTO training_export_passes \
             (export_set, ids_hash, file_path, staged_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(export_set, ids_hash) DO NOTHING",
            params![export_set, ids_hash, path, now_ms],
        )?;
        Ok(())
    }

    /// CORR PART 5: look up the staged export-file path for a
    /// prior pass with the same (`export_set`, `ids_hash`).
    /// Returns `None` when no prior staging row exists.
    pub fn lookup_staged_export(
        &self,
        export_set: &str,
        ids_hash: &str,
    ) -> Result<Option<String>, TrainingStoreError> {
        let conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        // Defensive: ensure the table exists. A fresh DB
        // calls this before stage_export_path has ever run.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS training_export_passes (\
                 export_set TEXT NOT NULL, \
                 ids_hash   TEXT NOT NULL, \
                 file_path  TEXT NOT NULL, \
                 staged_at  INTEGER NOT NULL, \
                 PRIMARY KEY (export_set, ids_hash) \
             )",
            [],
        )?;
        let mut stmt = conn.prepare(
            "SELECT file_path FROM training_export_passes \
             WHERE export_set = ?1 AND ids_hash = ?2",
        )?;
        let mut rows = stmt.query(params![export_set, ids_hash])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get::<_, String>(0)?))
        } else {
            Ok(None)
        }
    }

    /// Mark a batch of interactions as exported and stamp them
    /// with the export set name.
    ///
    /// CORR PART 5: the UPDATE now carries `WHERE exported = 0`
    /// so an already-exported row stays mapped to its prior
    /// export set rather than being silently re-claimed by a
    /// later export pass. The returned `usize` counts ONLY
    /// rows that transitioned from un-exported → exported
    /// (rows that were already exported contribute zero), so
    /// the exporter can distinguish "this row was claimed by
    /// us" from "this row was already claimed before".
    pub fn mark_exported(
        &self,
        ids: &[InteractionId],
        export_set: &str,
    ) -> Result<usize, TrainingStoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        // PART 2: collapse the per-id UPDATE loop into a single
        // statement that uses `WHERE interaction_id IN (?, ?, …)`.
        // Chunked at 500 to stay well under SQLite's per-statement
        // parameter cap (SQLITE_MAX_VARIABLE_NUMBER) across hosts.
        const CHUNK: usize = 500;
        let mut conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        let tx = conn.transaction()?;
        let mut n = 0usize;
        for chunk in ids.chunks(CHUNK) {
            // Bind positions: ?1 = export_set, ?2.. = each id.
            let placeholders: String = (0..chunk.len())
                .map(|i| format!("?{}", i + 2))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "UPDATE training_interactions SET exported = 1, export_set = ?1 \
                 WHERE interaction_id IN ({placeholders}) AND exported = 0"
            );
            let mut bind: Vec<rusqlite::types::Value> = Vec::with_capacity(1 + chunk.len());
            bind.push(rusqlite::types::Value::Text(export_set.to_string()));
            for id in chunk {
                bind.push(rusqlite::types::Value::Text(id.as_str().to_string()));
            }
            n += tx.execute(&sql, rusqlite::params_from_iter(bind.iter()))?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Paginated listing — used by `training.list_interactions`
    /// and the operator dashboard. Ordering is `recorded_at DESC`.
    pub fn list_summaries(
        &self,
        filters: &ListFilters,
        page: u32,
        page_size: u32,
    ) -> Result<Vec<InteractionSummary>, TrainingStoreError> {
        let page = page.max(1);
        let page_size = page_size.clamp(1, 500);
        let offset = (page - 1) as i64 * page_size as i64;
        let (where_clause, params_vec) = build_where(filters);
        let sql = format!(
            "SELECT interaction_id, session_id, agent, model, provider, \
                    user_message, latency_ms, success, error_kind, token_count, \
                    recorded_at, quality_score, exported, export_set, anonymized \
             FROM training_interactions {where_clause} \
             ORDER BY recorded_at DESC LIMIT ?{lim} OFFSET ?{off}",
            lim = params_vec.len() + 1,
            off = params_vec.len() + 2
        );
        let conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        let mut stmt = conn.prepare(&sql)?;
        let mut bind: Vec<rusqlite::types::Value> = params_vec.clone();
        bind.push(rusqlite::types::Value::Integer(page_size as i64));
        bind.push(rusqlite::types::Value::Integer(offset));
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bind.iter()), row_to_summary)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// List a batch of unscored interactions for the background
    /// scorer. Returns up to `limit` records oldest-first so the
    /// scorer makes monotonic progress across restarts.
    pub fn list_unscored(&self, limit: u32) -> Result<Vec<InteractionRecord>, TrainingStoreError> {
        let conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT interaction_id, session_id, agent, model, provider, \
                    system_prompt, user_message, response, tool_calls_json, \
                    token_count, prompt_tokens, completion_tokens, latency_ms, \
                    success, error_kind, recorded_at, quality_score, exported, export_set, \
                    anonymized \
             FROM training_interactions \
             WHERE quality_score IS NULL \
             ORDER BY recorded_at ASC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_record)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Materialise the full record set the export engine should
    /// emit. `max` caps the number of rows returned; when set,
    /// rows are ordered by `quality_score DESC` so the
    /// highest-scoring interactions land first. Without `max`,
    /// rows come back in `recorded_at DESC` order (newest first).
    pub fn list_for_export(
        &self,
        filters: &ListFilters,
        max: Option<u32>,
    ) -> Result<Vec<InteractionRecord>, TrainingStoreError> {
        let (where_clause, params_vec) = build_where(filters);
        let order = if max.is_some() {
            "ORDER BY quality_score DESC, recorded_at DESC"
        } else {
            "ORDER BY recorded_at DESC"
        };
        let limit_clause = if max.is_some() {
            format!("LIMIT ?{}", params_vec.len() + 1)
        } else {
            String::new()
        };
        let sql = format!(
            "SELECT interaction_id, session_id, agent, model, provider, \
                    system_prompt, user_message, response, tool_calls_json, \
                    token_count, prompt_tokens, completion_tokens, latency_ms, \
                    success, error_kind, recorded_at, quality_score, exported, export_set, \
                    anonymized \
             FROM training_interactions {where_clause} {order} {limit_clause}",
        );
        let conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        let mut stmt = conn.prepare(&sql)?;
        let mut bind: Vec<rusqlite::types::Value> = params_vec.clone();
        if let Some(m) = max {
            bind.push(rusqlite::types::Value::Integer(m as i64));
        }
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bind.iter()), row_to_record)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Aggregate stats payload returned by `training.stats`.
    pub fn stats(&self) -> Result<TrainingStats, TrainingStoreError> {
        let conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM training_interactions", [], |r| {
            r.get(0)
        })?;
        let exported: i64 = conn.query_row(
            "SELECT COUNT(*) FROM training_interactions WHERE exported = 1",
            [],
            |r| r.get(0),
        )?;
        let avg: Option<f64> = conn
            .query_row(
                "SELECT AVG(quality_score) FROM training_interactions WHERE quality_score IS NOT NULL",
                [],
                |r| r.get::<_, Option<f64>>(0),
            )
            .optional()?
            .flatten();

        let mut dist = ScoreDistribution::default();
        let mut stmt = conn.prepare("SELECT quality_score FROM training_interactions")?;
        let scores = stmt
            .query_map([], |r| r.get::<_, Option<f64>>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for s in &scores {
            match s {
                None => dist.unscored += 1,
                Some(v) => {
                    // Clamp into [0.0, 1.0] then bucket.
                    let clamped = v.clamp(0.0, 1.0);
                    let idx = ((clamped * 10.0) as usize).min(9);
                    dist.buckets[idx] += 1;
                }
            }
        }

        let by_agent = group_counts(&conn, "agent")?;
        let by_model = group_counts(&conn, "model")?;

        Ok(TrainingStats {
            total: total as u64,
            exported: exported as u64,
            average_quality_score: avg,
            score_distribution: dist,
            by_agent,
            by_model,
        })
    }

    /// Delete every row whose `recorded_at` is older than the
    /// `cutoff_ms` watermark. Returns the deletion count.
    pub fn prune_older_than(&self, cutoff_ms: i64) -> Result<u64, TrainingStoreError> {
        let conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        let n = conn.execute(
            "DELETE FROM training_interactions WHERE recorded_at < ?1",
            params![cutoff_ms],
        )?;
        Ok(n as u64)
    }

    pub fn row_count(&self) -> Result<u64, TrainingStoreError> {
        let conn = self.conn.lock().map_err(|_| TrainingStoreError::Lock)?;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM training_interactions", [], |r| {
            r.get(0)
        })?;
        Ok(n as u64)
    }
}

fn insert_one(
    conn: &Connection,
    rec: &InteractionRecord,
    tenant_id: &str,
) -> Result<(), TrainingStoreError> {
    let tool_calls_json = serde_json::to_string(&rec.tool_calls)
        .map_err(|e| TrainingStoreError::Encode(e.to_string()))?;
    let tenant = if tenant_id.trim().is_empty() {
        "default"
    } else {
        tenant_id
    };
    conn.execute(
        "INSERT OR REPLACE INTO training_interactions \
         (interaction_id, session_id, agent, model, provider, \
          system_prompt, user_message, response, tool_calls_json, \
          token_count, prompt_tokens, completion_tokens, latency_ms, \
          success, error_kind, recorded_at, quality_score, exported, export_set, anonymized, \
          tenant_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
        params![
            rec.interaction_id.as_str(),
            rec.session_id,
            rec.agent,
            rec.model,
            rec.provider,
            rec.system_prompt,
            rec.user_message,
            rec.response,
            tool_calls_json,
            rec.token_count.map(|v| v as i64),
            rec.prompt_tokens.map(|v| v as i64),
            rec.completion_tokens.map(|v| v as i64),
            rec.latency_ms as i64,
            rec.success as i32,
            rec.error_kind,
            rec.recorded_at,
            rec.quality_score.map(|v| v as f64),
            rec.exported as i32,
            rec.export_set,
            rec.anonymized as i32,
            tenant,
        ],
    )?;
    Ok(())
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<InteractionRecord> {
    let tool_calls_json: String = row.get(8)?;
    let tool_calls: Vec<ToolCallRecord> =
        serde_json::from_str(&tool_calls_json).unwrap_or_default();
    let interaction_id: String = row.get(0)?;
    Ok(InteractionRecord {
        interaction_id: InteractionId(interaction_id),
        session_id: row.get(1)?,
        agent: row.get(2)?,
        model: row.get(3)?,
        provider: row.get(4)?,
        system_prompt: row.get(5)?,
        user_message: row.get(6)?,
        response: row.get(7)?,
        tool_calls,
        token_count: row.get::<_, Option<i64>>(9)?.map(|v| v.max(0) as u32),
        prompt_tokens: row.get::<_, Option<i64>>(10)?.map(|v| v.max(0) as u32),
        completion_tokens: row.get::<_, Option<i64>>(11)?.map(|v| v.max(0) as u32),
        latency_ms: row.get::<_, i64>(12)?.max(0) as u64,
        success: row.get::<_, i32>(13)? != 0,
        error_kind: row.get(14)?,
        recorded_at: row.get(15)?,
        quality_score: row.get::<_, Option<f64>>(16)?.map(|v| v as f32),
        exported: row.get::<_, i32>(17)? != 0,
        export_set: row.get(18)?,
        anonymized: row.get::<_, i32>(19)? != 0,
    })
}

fn row_to_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<InteractionSummary> {
    let interaction_id: String = row.get(0)?;
    let user_message: String = row.get(5)?;
    let user_preview: String = user_message.chars().take(80).collect();
    Ok(InteractionSummary {
        interaction_id: InteractionId(interaction_id),
        session_id: row.get(1)?,
        agent: row.get(2)?,
        model: row.get(3)?,
        provider: row.get(4)?,
        latency_ms: row.get::<_, i64>(6)?.max(0) as u64,
        success: row.get::<_, i32>(7)? != 0,
        error_kind: row.get(8)?,
        token_count: row.get::<_, Option<i64>>(9)?.map(|v| v.max(0) as u32),
        recorded_at: row.get(10)?,
        quality_score: row.get::<_, Option<f64>>(11)?.map(|v| v as f32),
        exported: row.get::<_, i32>(12)? != 0,
        export_set: row.get(13)?,
        user_preview,
        anonymized: row.get::<_, i32>(14)? != 0,
    })
}

fn build_where(filters: &ListFilters) -> (String, Vec<rusqlite::types::Value>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(a) = filters.agent.as_ref() {
        let idx = params.len() + 1;
        clauses.push(format!("agent = ?{idx}"));
        params.push(rusqlite::types::Value::Text(a.clone()));
    }
    if let Some(s) = filters.session_id.as_ref() {
        let idx = params.len() + 1;
        clauses.push(format!("session_id = ?{idx}"));
        params.push(rusqlite::types::Value::Text(s.clone()));
    }
    if let Some(m) = filters.model.as_ref() {
        let idx = params.len() + 1;
        clauses.push(format!("model = ?{idx}"));
        params.push(rusqlite::types::Value::Text(m.clone()));
    }
    if let Some(s) = filters.min_quality_score {
        let idx = params.len() + 1;
        clauses.push(format!(
            "quality_score IS NOT NULL AND quality_score >= ?{idx}"
        ));
        params.push(rusqlite::types::Value::Real(s as f64));
    } else if filters.require_scored {
        clauses.push("quality_score IS NOT NULL".into());
    }
    if let Some(from) = filters.date_from {
        let idx = params.len() + 1;
        clauses.push(format!("recorded_at >= ?{idx}"));
        params.push(rusqlite::types::Value::Integer(from));
    }
    if let Some(to) = filters.date_to {
        let idx = params.len() + 1;
        clauses.push(format!("recorded_at <= ?{idx}"));
        params.push(rusqlite::types::Value::Integer(to));
    }
    if let Some(exp) = filters.exported {
        let idx = params.len() + 1;
        clauses.push(format!("exported = ?{idx}"));
        params.push(rusqlite::types::Value::Integer(exp as i64));
    }
    let clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    (clause, params)
}

fn group_counts(conn: &Connection, column: &str) -> Result<Vec<GroupedCount>, TrainingStoreError> {
    let sql = format!(
        "SELECT {column} AS label, COUNT(*) AS n FROM training_interactions \
         GROUP BY {column} ORDER BY n DESC, label ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| {
            Ok(GroupedCount {
                label: r.get(0)?,
                count: r.get::<_, i64>(1)?.max(0) as u64,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn init_schema(conn: &Connection) -> Result<(), TrainingStoreError> {
    // CORR PART 2: register schema with the identifier-based
    // migration framework. Legacy claim makes pre-fix DBs
    // stamp v1 without re-CREATE; the body migration runs
    // only on a fresh DB. The post-create ALTER TABLE for
    // `anonymized` is kept below to remain idempotent against
    // any DB seen in production.
    crate::db::claim_legacy_migration(conn, "training_store.v1", |c| {
        let n: i64 = c.query_row(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type='table' AND name='training_interactions'",
            [],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    })
    .map_err(|e| TrainingStoreError::Db(e.to_string()))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS training_interactions (\
             interaction_id    TEXT PRIMARY KEY,\
             session_id        TEXT NOT NULL,\
             agent             TEXT NOT NULL,\
             model             TEXT NOT NULL,\
             provider          TEXT NOT NULL,\
             system_prompt     TEXT NOT NULL,\
             user_message      TEXT NOT NULL,\
             response          TEXT NOT NULL,\
             tool_calls_json   TEXT NOT NULL,\
             token_count       INTEGER,\
             prompt_tokens     INTEGER,\
             completion_tokens INTEGER,\
             latency_ms        INTEGER NOT NULL,\
             success           INTEGER NOT NULL,\
             error_kind        TEXT,\
             recorded_at       INTEGER NOT NULL,\
             quality_score     REAL,\
             exported          INTEGER NOT NULL DEFAULT 0,\
             export_set        TEXT,\
             anonymized        INTEGER NOT NULL DEFAULT 0\
         );\
         CREATE INDEX IF NOT EXISTS training_interactions_session \
             ON training_interactions(session_id);\
         CREATE INDEX IF NOT EXISTS training_interactions_agent \
             ON training_interactions(agent);\
         CREATE INDEX IF NOT EXISTS training_interactions_ts \
             ON training_interactions(recorded_at DESC);\
         CREATE INDEX IF NOT EXISTS training_interactions_score \
             ON training_interactions(quality_score);\
         CREATE INDEX IF NOT EXISTS training_interactions_export \
             ON training_interactions(exported, quality_score DESC);\
         CREATE INDEX IF NOT EXISTS training_interactions_anonymized \
             ON training_interactions(anonymized);",
    )?;
    // Backwards-compat: if a pre-7.15-PII database (no
    // `anonymized` column) opens against this build, the
    // CREATE TABLE IF NOT EXISTS above is a no-op and we
    // need to add the column in place. SQLite has no
    // `ADD COLUMN IF NOT EXISTS`, so we probe `PRAGMA
    // table_info` first.
    if !column_exists(conn, "training_interactions", "anonymized")? {
        conn.execute_batch(
            "ALTER TABLE training_interactions \
             ADD COLUMN anonymized INTEGER NOT NULL DEFAULT 0;\
             CREATE INDEX IF NOT EXISTS training_interactions_anonymized \
                 ON training_interactions(anonymized);",
        )?;
    }
    // GROUP 6: tenant isolation. Idempotent via the column probe;
    // pre-migration rows default to the reserved 'default' tenant
    // (safe — single-tenant deployments read as "default").
    if !column_exists(conn, "training_interactions", "tenant_id")? {
        conn.execute_batch(
            "ALTER TABLE training_interactions \
             ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default';\
             CREATE INDEX IF NOT EXISTS training_interactions_tenant \
                 ON training_interactions(tenant_id, session_id);",
        )?;
    }
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, TrainingStoreError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for r in rows {
        let name = r?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Default location for the training database next to the rest
/// of a controller's data dir.
pub fn default_training_path(data_dir: &Path) -> PathBuf {
    data_dir.join("training.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group6_training_reads_are_isolated_by_verified_tenant() {
        // Two tenants record interactions under the SAME
        // session_id (the cross-tenant shared key). A read scoped
        // to tenant A must see ONLY A's interaction.
        let store = TrainingStore::in_memory().unwrap();
        let mut a = sample("ia", "alice", 1, true);
        a.session_id = "shared-session".into();
        let mut b = sample("ib", "bob", 2, true);
        b.session_id = "shared-session".into();
        store.insert_for_tenant(&a, "tenant-a").unwrap();
        store.insert_for_tenant(&b, "tenant-b").unwrap();
        assert_eq!(
            store
                .count_for_tenant_and_session("tenant-a", "shared-session")
                .unwrap(),
            1,
            "tenant A must see only its own interaction for the shared session"
        );
        assert_eq!(
            store
                .count_for_tenant_and_session("tenant-b", "shared-session")
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .count_for_tenant_and_session("tenant-c", "shared-session")
                .unwrap(),
            0
        );
    }

    fn sample(id: &str, agent: &str, ts: i64, success: bool) -> InteractionRecord {
        InteractionRecord {
            interaction_id: InteractionId(id.to_string()),
            session_id: "s1".into(),
            agent: agent.into(),
            model: "gpt-4o-mini".into(),
            provider: "openai".into(),
            system_prompt: "you are alice".into(),
            user_message: "hi alice".into(),
            response: "hi human".into(),
            tool_calls: vec![ToolCallRecord {
                tool: "web_fetch".into(),
                input: "https://x".into(),
                output: "ok".into(),
                success: true,
                latency_ms: 20,
                error_kind: None,
            }],
            token_count: Some(100),
            prompt_tokens: Some(40),
            completion_tokens: Some(60),
            latency_ms: 200,
            success,
            error_kind: if success {
                None
            } else {
                Some("RESPONDER_INTERNAL".into())
            },
            recorded_at: ts,
            quality_score: None,
            exported: false,
            export_set: None,
            anonymized: false,
        }
    }

    #[test]
    fn insert_and_get_round_trip() {
        let store = TrainingStore::in_memory().unwrap();
        let rec = sample("aaaa", "alice", 100, true);
        store.insert(&rec).unwrap();
        let got = store.get("aaaa").unwrap().unwrap();
        assert_eq!(got.interaction_id.as_str(), "aaaa");
        assert_eq!(got.tool_calls.len(), 1);
        assert_eq!(got.tool_calls[0].tool, "web_fetch");
        assert_eq!(got.token_count, Some(100));
    }

    #[test]
    fn insert_batch_writes_all_rows() {
        let store = TrainingStore::in_memory().unwrap();
        let batch: Vec<_> = (0..25)
            .map(|i| sample(&format!("id{i:04}"), "alice", 100 + i, true))
            .collect();
        store.insert_batch(&batch).unwrap();
        assert_eq!(store.row_count().unwrap(), 25);
    }

    #[test]
    fn delete_returns_true_on_hit_false_on_miss() {
        let store = TrainingStore::in_memory().unwrap();
        store.insert(&sample("zzz", "alice", 100, true)).unwrap();
        assert!(store.delete("zzz").unwrap());
        assert!(!store.delete("zzz").unwrap());
    }

    #[test]
    fn set_quality_score_updates_column() {
        let store = TrainingStore::in_memory().unwrap();
        store.insert(&sample("a1", "alice", 100, true)).unwrap();
        assert!(store.set_quality_score("a1", Some(0.83)).unwrap());
        let got = store.get("a1").unwrap().unwrap();
        assert!((got.quality_score.unwrap() - 0.83).abs() < 1e-4);
    }

    #[test]
    fn list_unscored_returns_only_null_score_rows() {
        let store = TrainingStore::in_memory().unwrap();
        store.insert(&sample("a1", "alice", 100, true)).unwrap();
        store.insert(&sample("a2", "alice", 200, true)).unwrap();
        store.set_quality_score("a1", Some(0.5)).unwrap();
        let unscored = store.list_unscored(10).unwrap();
        assert_eq!(unscored.len(), 1);
        assert_eq!(unscored[0].interaction_id.as_str(), "a2");
    }

    #[test]
    fn list_summaries_paginates_newest_first() {
        let store = TrainingStore::in_memory().unwrap();
        for i in 0..10 {
            store
                .insert(&sample(&format!("id{i}"), "alice", 100 + i, true))
                .unwrap();
        }
        let p1 = store.list_summaries(&ListFilters::default(), 1, 3).unwrap();
        assert_eq!(p1.len(), 3);
        // Newest first: id9, id8, id7
        assert_eq!(p1[0].interaction_id.as_str(), "id9");
        assert_eq!(p1[2].interaction_id.as_str(), "id7");
        let p2 = store.list_summaries(&ListFilters::default(), 2, 3).unwrap();
        assert_eq!(p2[0].interaction_id.as_str(), "id6");
    }

    #[test]
    fn filter_by_agent_excludes_other_agents() {
        let store = TrainingStore::in_memory().unwrap();
        store.insert(&sample("a", "alice", 100, true)).unwrap();
        store.insert(&sample("b", "bob", 200, true)).unwrap();
        let f = ListFilters {
            agent: Some("alice".into()),
            ..ListFilters::default()
        };
        let s = store.list_summaries(&f, 1, 50).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].agent, "alice");
    }

    #[test]
    fn filter_by_quality_excludes_low_scoring() {
        let store = TrainingStore::in_memory().unwrap();
        store.insert(&sample("a", "alice", 100, true)).unwrap();
        store.insert(&sample("b", "alice", 200, true)).unwrap();
        store.set_quality_score("a", Some(0.9)).unwrap();
        store.set_quality_score("b", Some(0.5)).unwrap();
        let f = ListFilters {
            min_quality_score: Some(0.7),
            ..ListFilters::default()
        };
        let s = store.list_summaries(&f, 1, 50).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].interaction_id.as_str(), "a");
    }

    #[test]
    fn list_for_export_with_max_takes_highest_scoring_first() {
        let store = TrainingStore::in_memory().unwrap();
        for i in 0..5 {
            store
                .insert(&sample(&format!("id{i}"), "alice", 100 + i, true))
                .unwrap();
            store
                .set_quality_score(&format!("id{i}"), Some(i as f32 / 10.0))
                .unwrap();
        }
        let rows = store
            .list_for_export(&ListFilters::default(), Some(2))
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].interaction_id.as_str(), "id4"); // score 0.4
        assert_eq!(rows[1].interaction_id.as_str(), "id3");
    }

    #[test]
    fn mark_exported_sets_columns() {
        let store = TrainingStore::in_memory().unwrap();
        store.insert(&sample("a", "alice", 100, true)).unwrap();
        let n = store
            .mark_exported(&[InteractionId("a".into())], "set1")
            .unwrap();
        assert_eq!(n, 1);
        let got = store.get("a").unwrap().unwrap();
        assert!(got.exported);
        assert_eq!(got.export_set.as_deref(), Some("set1"));
    }

    // ── CORR PART 5: mark_exported guard + staging ─────────

    #[test]
    fn corr_p5_mark_exported_does_not_overwrite_already_exported_row() {
        let store = TrainingStore::in_memory().unwrap();
        store.insert(&sample("a", "alice", 100, true)).unwrap();
        let n1 = store
            .mark_exported(&[InteractionId("a".into())], "set1")
            .unwrap();
        assert_eq!(n1, 1);
        // Second pass with a different set name MUST NOT
        // reclaim the row.
        let n2 = store
            .mark_exported(&[InteractionId("a".into())], "set2")
            .unwrap();
        assert_eq!(n2, 0, "second pass must not reclaim");
        let got = store.get("a").unwrap().unwrap();
        assert_eq!(
            got.export_set.as_deref(),
            Some("set1"),
            "original set name preserved"
        );
    }

    #[test]
    fn corr_p5_stage_export_path_is_idempotent() {
        let store = TrainingStore::in_memory().unwrap();
        store
            .stage_export_path("setA", "abc123", "/tmp/out.jsonl")
            .unwrap();
        store
            .stage_export_path("setA", "abc123", "/tmp/another.jsonl")
            .unwrap();
        let path = store.lookup_staged_export("setA", "abc123").unwrap();
        assert_eq!(path.as_deref(), Some("/tmp/out.jsonl"));
    }

    #[test]
    fn prune_older_than_drops_old_rows() {
        let store = TrainingStore::in_memory().unwrap();
        store.insert(&sample("old", "alice", 100, true)).unwrap();
        store
            .insert(&sample("new", "alice", 9_999_999, true))
            .unwrap();
        let n = store.prune_older_than(1_000).unwrap();
        assert_eq!(n, 1);
        assert!(store.get("old").unwrap().is_none());
        assert!(store.get("new").unwrap().is_some());
    }

    #[test]
    fn stats_groups_by_agent_and_distributes_scores() {
        let store = TrainingStore::in_memory().unwrap();
        store.insert(&sample("a", "alice", 100, true)).unwrap();
        store.insert(&sample("b", "alice", 200, true)).unwrap();
        store.insert(&sample("c", "bob", 300, true)).unwrap();
        store.set_quality_score("a", Some(0.95)).unwrap();
        store.set_quality_score("b", Some(0.05)).unwrap();
        // "c" stays unscored.
        store
            .mark_exported(&[InteractionId("a".into())], "set1")
            .unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats.total, 3);
        assert_eq!(stats.exported, 1);
        // alice=2, bob=1
        let alice = stats.by_agent.iter().find(|g| g.label == "alice").unwrap();
        assert_eq!(alice.count, 2);
        // Score 0.95 lands in bucket 9; 0.05 in bucket 0.
        assert_eq!(stats.score_distribution.buckets[0], 1);
        assert_eq!(stats.score_distribution.buckets[9], 1);
        assert_eq!(stats.score_distribution.unscored, 1);
        let avg = stats.average_quality_score.unwrap();
        assert!((avg - 0.5).abs() < 1e-4);
    }

    #[test]
    fn stats_handles_all_unscored_database() {
        let store = TrainingStore::in_memory().unwrap();
        store.insert(&sample("a", "alice", 100, true)).unwrap();
        let stats = store.stats().unwrap();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.exported, 0);
        assert!(stats.average_quality_score.is_none());
        assert_eq!(stats.score_distribution.unscored, 1);
    }
}
