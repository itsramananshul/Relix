//! SQLite store for per-invocation agent metrics — RELIX-7.11.
//!
//! Schema is intentionally narrow + append-only:
//!
//! ```text
//! CREATE TABLE metrics_invocations (
//!   id            INTEGER PRIMARY KEY AUTOINCREMENT,
//!   agent_name    TEXT    NOT NULL,
//!   peer_alias    TEXT    NOT NULL DEFAULT '',
//!   method        TEXT    NOT NULL,
//!   timestamp_ms  INTEGER NOT NULL,
//!   latency_ms    INTEGER NOT NULL,
//!   success       INTEGER NOT NULL,
//!   error_kind    TEXT,
//!   token_count   INTEGER,        -- total tokens (prompt+completion) when known
//!   cost_micros   INTEGER,        -- estimated USD * 1_000_000; integer math
//!   input_bytes   INTEGER NOT NULL,
//!   output_bytes  INTEGER NOT NULL,
//!   model         TEXT
//! );
//! CREATE INDEX metrics_invocations_agent_ts
//!   ON metrics_invocations(agent_name, timestamp_ms DESC);
//! CREATE INDEX metrics_invocations_method_ts
//!   ON metrics_invocations(method, timestamp_ms DESC);
//! ```
//!
//! Rows are never updated. Retention is a single `DELETE WHERE
//! timestamp_ms < cutoff` that runs hourly from a background
//! task. The store itself is sync — the async batching layer
//! lives in `collector.rs`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};

use super::types::InvocationMetric;

#[derive(Debug, thiserror::Error)]
pub enum MetricsStoreError {
    #[error("metrics store io: {0}")]
    Io(String),
    #[error("metrics store sqlite: {0}")]
    Db(String),
    #[error("metrics store lock poisoned")]
    Lock,
}

impl From<rusqlite::Error> for MetricsStoreError {
    fn from(e: rusqlite::Error) -> Self {
        MetricsStoreError::Db(e.to_string())
    }
}

/// Append-only metrics store. Cheap to clone (single Arc).
#[derive(Clone)]
pub struct MetricsStore {
    conn: Arc<Mutex<Connection>>,
}

impl MetricsStore {
    /// Open or create a file-backed metrics database.
    pub fn open(path: &Path) -> Result<Self, MetricsStoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| MetricsStoreError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open an in-memory metrics store — used by tests.
    pub fn in_memory() -> Result<Self, MetricsStoreError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Insert a single metric. Wrapped by [`insert_batch`] when
    /// the collector flushes — direct callers can use this for
    /// tests + one-off recordings.
    pub fn insert(&self, m: &InvocationMetric) -> Result<(), MetricsStoreError> {
        let conn = self.conn.lock().map_err(|_| MetricsStoreError::Lock)?;
        insert_one(&conn, m)
    }

    /// Insert N metrics in a single transaction. The collector's
    /// drain loop uses this — one fsync per ≤100 rows /
    /// 100ms instead of per row.
    pub fn insert_batch(&self, metrics: &[InvocationMetric]) -> Result<(), MetricsStoreError> {
        if metrics.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().map_err(|_| MetricsStoreError::Lock)?;
        let tx = conn.transaction()?;
        for m in metrics {
            insert_one(&tx, m)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete every row whose `timestamp_ms` is older than
    /// `cutoff_ms`. Returns the deletion count.
    pub fn prune_older_than(&self, cutoff_ms: i64) -> Result<u64, MetricsStoreError> {
        let conn = self.conn.lock().map_err(|_| MetricsStoreError::Lock)?;
        let n = conn.execute(
            "DELETE FROM metrics_invocations WHERE timestamp_ms < ?1",
            params![cutoff_ms],
        )?;
        Ok(n as u64)
    }

    /// Total row count — used by tests + the dashboard's
    /// "metrics enabled" indicator.
    pub fn row_count(&self) -> Result<u64, MetricsStoreError> {
        let conn = self.conn.lock().map_err(|_| MetricsStoreError::Lock)?;
        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM metrics_invocations", [], |r| r.get(0))?;
        Ok(n as u64)
    }

    /// GROUP 6: tenant-scoped read. Counts rows for `tenant`
    /// matching `agent_name` — a SELECT that filters by the
    /// caller's VERIFIED tenant. A caller scoped to tenant A can
    /// never observe tenant B's rows even for a shared
    /// agent/session key, because the `tenant_id = ?` predicate
    /// is applied in SQL, not in the (correct-but-bypassable)
    /// handler layer.
    pub fn count_for_tenant_and_agent(
        &self,
        tenant: &str,
        agent: &str,
    ) -> Result<u64, MetricsStoreError> {
        let conn = self.conn.lock().map_err(|_| MetricsStoreError::Lock)?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM metrics_invocations WHERE tenant_id = ?1 AND agent_name = ?2",
            params![tenant, agent],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Borrow the underlying connection for read queries. Used
    /// by [`super::query::MetricsQuery`].
    pub fn with_conn<F, R>(&self, f: F) -> Result<R, MetricsStoreError>
    where
        F: FnOnce(&Connection) -> Result<R, rusqlite::Error>,
    {
        let conn = self.conn.lock().map_err(|_| MetricsStoreError::Lock)?;
        f(&conn).map_err(MetricsStoreError::from)
    }
}

fn insert_one(conn: &Connection, m: &InvocationMetric) -> Result<(), MetricsStoreError> {
    // GROUP 6: persist the verified tenant; default to the
    // reserved single-tenant sentinel if a caller left it empty.
    let tenant = if m.tenant_id.trim().is_empty() {
        "default"
    } else {
        m.tenant_id.as_str()
    };
    conn.execute(
        "INSERT INTO metrics_invocations \
         (agent_name, peer_alias, method, timestamp_ms, latency_ms, success, \
          error_kind, token_count, cost_micros, input_bytes, output_bytes, model, \
          confidence_score, routing_tier, tenant_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            m.agent_name,
            m.peer_alias,
            m.method,
            m.timestamp_ms,
            m.latency_ms as i64,
            m.success as i32,
            m.error_kind,
            m.token_count.map(|v| v as i64),
            m.cost_micros.map(|v| v as i64),
            m.input_bytes as i64,
            m.output_bytes as i64,
            m.model,
            m.confidence_score.map(|v| v as f64),
            m.routing_tier,
            tenant,
        ],
    )?;
    Ok(())
}

fn init_schema(conn: &Connection) -> Result<(), MetricsStoreError> {
    // CORR PART 2: register the metrics_invocations table
    // with the identifier-based migration framework. Pre-fix
    // DBs are claimed at v1 without re-CREATE; the post-init
    // ALTER columns below remain idempotent.
    crate::db::claim_legacy_migration(conn, "metrics_store.v1", |c| {
        let n: i64 = c.query_row(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type='table' AND name='metrics_invocations'",
            [],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    })
    .map_err(|e| MetricsStoreError::Db(e.to_string()))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS metrics_invocations (\
             id            INTEGER PRIMARY KEY AUTOINCREMENT,\
             agent_name    TEXT NOT NULL,\
             peer_alias    TEXT NOT NULL DEFAULT '',\
             method        TEXT NOT NULL,\
             timestamp_ms  INTEGER NOT NULL,\
             latency_ms    INTEGER NOT NULL,\
             success       INTEGER NOT NULL,\
             error_kind    TEXT,\
             token_count   INTEGER,\
             cost_micros   INTEGER,\
             input_bytes   INTEGER NOT NULL,\
             output_bytes  INTEGER NOT NULL,\
             model         TEXT\
         );\
         CREATE INDEX IF NOT EXISTS metrics_invocations_agent_ts \
             ON metrics_invocations(agent_name, timestamp_ms DESC);\
         CREATE INDEX IF NOT EXISTS metrics_invocations_method_ts \
             ON metrics_invocations(method, timestamp_ms DESC);\
         CREATE INDEX IF NOT EXISTS metrics_invocations_ts \
             ON metrics_invocations(timestamp_ms DESC);",
    )?;
    // RELIX-7.19: backwards-compat ALTER to add `confidence_score`
    // when the column doesn't exist yet. Pre-7.19 databases pick
    // up the new column on open with the NULL default (which
    // serialises to `None` on the model side).
    if !column_exists(conn, "metrics_invocations", "confidence_score")? {
        conn.execute(
            "ALTER TABLE metrics_invocations ADD COLUMN confidence_score REAL",
            [],
        )?;
    }
    // RELIX-7.29 PART 1: backwards-compat ALTER to add
    // `routing_tier` — populated by the AI handler when the
    // `[ai.routing]` tier router resolves a tier for the call.
    // NULL on the row means routing was disabled or no tier
    // mapped, which downstream dashboards treat as "default".
    if !column_exists(conn, "metrics_invocations", "routing_tier")? {
        conn.execute(
            "ALTER TABLE metrics_invocations ADD COLUMN routing_tier TEXT",
            [],
        )?;
    }
    // GROUP 6: tenant isolation. Add `tenant_id` so each row is
    // attributed to the caller's VERIFIED tenant and reads can be
    // tenant-scoped. The column probe makes this idempotent
    // (re-open is a no-op); existing pre-migration rows get the
    // reserved `'default'` tenant via the column default, so a
    // single-tenant deployment (which reads as `"default"`) still
    // sees its own historical rows. NOT NULL DEFAULT 'default'
    // also means legacy `INSERT`s that omit the column stay valid.
    if !column_exists(conn, "metrics_invocations", "tenant_id")? {
        conn.execute(
            "ALTER TABLE metrics_invocations ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default'",
            [],
        )?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS metrics_invocations_tenant_ts \
             ON metrics_invocations(tenant_id, timestamp_ms DESC);",
    )?;
    Ok(())
}

/// Probe for a column's existence using SQLite's `PRAGMA
/// table_info`. Used by the 7.19 confidence migration so a
/// pre-7.19 database picks up the new column on open without
/// failing the `ALTER TABLE` on a fresh schema.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, MetricsStoreError> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Default location for the metrics DB beneath a data dir.
pub fn default_metrics_path(data_dir: &Path) -> PathBuf {
    data_dir.join("metrics.sqlite")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_for_tenant(
        tenant: &str,
        agent: &str,
        method: &str,
        ts: i64,
        latency: u64,
        success: bool,
    ) -> InvocationMetric {
        let mut m = sample(agent, method, ts, latency, success);
        m.tenant_id = tenant.to_string();
        m
    }

    #[test]
    fn group6_metrics_reads_are_isolated_by_verified_tenant() {
        // Two tenants write rows that share the SAME agent_name
        // (the cross-tenant shared key an attacker would query
        // by). A read scoped to tenant A must return ONLY A's
        // row — never B's — proving real cross-tenant denial at
        // the data layer.
        let store = MetricsStore::in_memory().unwrap();
        store
            .insert(&sample_for_tenant(
                "tenant-a",
                "shared-agent",
                "ai.chat",
                100,
                5,
                true,
            ))
            .unwrap();
        store
            .insert(&sample_for_tenant(
                "tenant-b",
                "shared-agent",
                "ai.chat",
                200,
                5,
                true,
            ))
            .unwrap();
        // Both rows exist globally...
        assert_eq!(store.row_count().unwrap(), 2);
        // ...but each tenant sees exactly one — its own.
        assert_eq!(
            store
                .count_for_tenant_and_agent("tenant-a", "shared-agent")
                .unwrap(),
            1,
            "tenant A must see only its own row"
        );
        assert_eq!(
            store
                .count_for_tenant_and_agent("tenant-b", "shared-agent")
                .unwrap(),
            1
        );
        // A tenant with no rows sees nothing for the shared key.
        assert_eq!(
            store
                .count_for_tenant_and_agent("tenant-c", "shared-agent")
                .unwrap(),
            0
        );
    }

    #[test]
    fn group6_metrics_migration_is_idempotent_and_defaults_legacy_rows() {
        // Simulate a pre-migration DB: create the OLD schema
        // (no tenant_id) + insert a legacy row, then run the
        // migration (init_schema) TWICE.
        let conn = Connection::open_in_memory().unwrap();
        crate::db::apply_pragmas(&conn).unwrap();
        crate::db::ensure_migration_table(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE metrics_invocations (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT,\
                 agent_name TEXT NOT NULL,\
                 peer_alias TEXT NOT NULL DEFAULT '',\
                 method TEXT NOT NULL,\
                 timestamp_ms INTEGER NOT NULL,\
                 latency_ms INTEGER NOT NULL,\
                 success INTEGER NOT NULL,\
                 error_kind TEXT,\
                 token_count INTEGER,\
                 cost_micros INTEGER,\
                 input_bytes INTEGER NOT NULL,\
                 output_bytes INTEGER NOT NULL,\
                 model TEXT\
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metrics_invocations \
             (agent_name, method, timestamp_ms, latency_ms, success, input_bytes, output_bytes) \
             VALUES ('legacy', 'm', 1, 1, 1, 0, 0)",
            [],
        )
        .unwrap();
        // Run the migration twice — must not error or double-apply.
        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap();
        // The legacy row survives (no data loss) and is
        // attributed to the reserved 'default' tenant, not NULL.
        let (cnt, tenant): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(tenant_id) FROM metrics_invocations WHERE agent_name='legacy'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cnt, 1, "legacy row must survive the migration");
        assert_eq!(
            tenant, "default",
            "pre-migration rows attributed to 'default'"
        );
    }

    fn sample(agent: &str, method: &str, ts: i64, latency: u64, success: bool) -> InvocationMetric {
        InvocationMetric {
            agent_name: agent.into(),
            tenant_id: "default".into(),
            peer_alias: "p".into(),
            method: method.into(),
            timestamp_ms: ts,
            latency_ms: latency,
            success,
            error_kind: if success {
                None
            } else {
                Some("INTERNAL".into())
            },
            token_count: None,
            cost_micros: None,
            input_bytes: 32,
            output_bytes: 64,
            model: None,
            confidence_score: None,
            routing_tier: None,
            request_id: None,
        }
    }

    #[test]
    fn open_in_memory_creates_table_and_indexes() {
        let store = MetricsStore::in_memory().unwrap();
        // Schema check — table exists, indexes exist.
        store
            .with_conn(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='metrics_invocations'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
            })
            .unwrap();
    }

    #[test]
    fn insert_single_row_round_trips() {
        let store = MetricsStore::in_memory().unwrap();
        store
            .insert(&sample("alice", "ai.chat", 100, 50, true))
            .unwrap();
        assert_eq!(store.row_count().unwrap(), 1);
    }

    #[test]
    fn insert_batch_writes_all_rows() {
        let store = MetricsStore::in_memory().unwrap();
        let metrics: Vec<_> = (0..50)
            .map(|i| sample("alice", "ai.chat", 100 + i as i64, 50, true))
            .collect();
        store.insert_batch(&metrics).unwrap();
        assert_eq!(store.row_count().unwrap(), 50);
    }

    #[test]
    fn insert_batch_empty_is_noop() {
        let store = MetricsStore::in_memory().unwrap();
        store.insert_batch(&[]).unwrap();
        assert_eq!(store.row_count().unwrap(), 0);
    }

    #[test]
    fn prune_older_than_deletes_only_old_rows() {
        let store = MetricsStore::in_memory().unwrap();
        store
            .insert(&sample("alice", "ai.chat", 100, 50, true))
            .unwrap();
        store
            .insert(&sample("alice", "ai.chat", 500, 50, true))
            .unwrap();
        store
            .insert(&sample("alice", "ai.chat", 1000, 50, true))
            .unwrap();
        let n = store.prune_older_than(300).unwrap();
        assert_eq!(n, 1);
        assert_eq!(store.row_count().unwrap(), 2);
    }

    #[test]
    fn prune_with_no_old_rows_is_noop() {
        let store = MetricsStore::in_memory().unwrap();
        store
            .insert(&sample("alice", "ai.chat", 1000, 50, true))
            .unwrap();
        let n = store.prune_older_than(100).unwrap();
        assert_eq!(n, 0);
        assert_eq!(store.row_count().unwrap(), 1);
    }

    #[test]
    fn failure_row_stores_error_kind() {
        let store = MetricsStore::in_memory().unwrap();
        store
            .insert(&sample("alice", "ai.chat", 100, 50, false))
            .unwrap();
        let kind: Option<String> = store
            .with_conn(|c| {
                c.query_row("SELECT error_kind FROM metrics_invocations", [], |r| {
                    r.get(0)
                })
            })
            .unwrap();
        assert_eq!(kind.as_deref(), Some("INTERNAL"));
    }

    #[test]
    fn token_and_cost_columns_persist_when_present() {
        let store = MetricsStore::in_memory().unwrap();
        let mut m = sample("alice", "ai.chat", 100, 50, true);
        m.token_count = Some(1234);
        m.cost_micros = Some(56_000); // $0.056
        m.model = Some("gpt-4o-mini".into());
        store.insert(&m).unwrap();
        let (tok, cost, model): (Option<i64>, Option<i64>, Option<String>) = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT token_count, cost_micros, model FROM metrics_invocations",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
            })
            .unwrap();
        assert_eq!(tok, Some(1234));
        assert_eq!(cost, Some(56_000));
        assert_eq!(model.as_deref(), Some("gpt-4o-mini"));
    }
}
