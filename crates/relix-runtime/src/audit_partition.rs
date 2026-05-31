//! GAP 23C — per-tenant audit log partitioning.
//!
//! A SQLite-backed mirror of every audit write the bridge
//! produces, keyed by tenant id. The canonical signed CBOR
//! audit log (in `relix-core::audit`) stays unchanged so its
//! hash chain remains backwards-compatible; this mirror is an
//! additional, queryable view operators can slice by tenant
//! without trawling the binary log.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS audit_partition (
//!   tenant_id        TEXT    NOT NULL,
//!   ts_secs          INTEGER NOT NULL,
//!   request_id       TEXT    NOT NULL,
//!   caller_name      TEXT    NOT NULL,
//!   method           TEXT    NOT NULL,
//!   policy_decision  TEXT    NOT NULL,
//!   status           TEXT    NOT NULL,
//!   error_kind       INTEGER,
//!   latency_ms       INTEGER NOT NULL,
//!   PRIMARY KEY (tenant_id, ts_secs, request_id)
//! );
//! CREATE INDEX IF NOT EXISTS idx_audit_partition_ts
//!   ON audit_partition(tenant_id, ts_secs DESC);
//! ```
//!
//! Rows with no tenant header land under the literal tenant id
//! `"default"` so `audit.tenant_list` always returns at least
//! that bucket once any traffic has flowed.
//!
//! ## Honest scope
//!
//! - This mirror is best-effort. A write failure is logged at
//!   `warn!` and the canonical CBOR log still finalises — the
//!   signed chain stays the source of truth for compliance
//!   audits.
//! - No deletes, no compaction; operators are expected to
//!   roll the file periodically (same lifecycle as the
//!   canonical log).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, params};

/// One row written to the partition mirror per finalised
/// audit record.
#[derive(Clone, Debug)]
pub struct PartitionRow {
    pub ts_secs: i64,
    pub request_id_hex: String,
    pub tenant_id: Option<String>,
    pub caller_name: String,
    pub method: String,
    pub policy_decision: String,
    pub status: &'static str,
    pub error_kind: Option<u32>,
    pub latency_ms: u64,
}

/// One read-back row surfaced through the `audit.tenant_recent`
/// cap (and the bridge proxy).
#[derive(Clone, Debug, serde::Serialize)]
pub struct PartitionReadRow {
    pub ts_secs: i64,
    pub request_id: String,
    pub tenant_id: String,
    pub caller_name: String,
    pub method: String,
    pub policy_decision: String,
    pub status: String,
    pub error_kind: Option<u32>,
    pub latency_ms: u64,
}

/// The mirror store. Holds a single connection guarded by a
/// mutex — writes are infrequent (one per dispatched call) and
/// SQLite's WAL mode + a serialised writer keeps contention
/// low. Cheap to clone the [`Arc`] callers wrap around it.
pub struct AuditPartitionStore {
    path: PathBuf,
    conn: Mutex<Connection>,
    /// PART 4: when `true`, every `append` MUST supply a
    /// non-empty `tenant_id`. A missing tenant returns
    /// `Err("audit_partition: tenant_id required …")` rather
    /// than the pre-PART-4 silent fall-through to the
    /// `"default"` bucket. Operators enable this in
    /// multi-tenant deployments so audit rows for tenant A
    /// can never be silently mixed into tenant B's bucket on
    /// a missing-header bug.
    partition_by_tenant: bool,
}

impl AuditPartitionStore {
    /// Open or create the partition store at `path`. Creates
    /// parent directories. Idempotent — the schema migration
    /// runs every open. `partition_by_tenant = false` so
    /// existing single-tenant callers stay byte-identical;
    /// callers opt into fail-closed via
    /// [`Self::open_with_partition`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        Self::open_with_partition(path, false)
    }

    /// PART 4 variant. When `partition_by_tenant = true`,
    /// `append` rejects rows with no tenant id rather than
    /// silently filing them under `"default"`.
    pub fn open_with_partition(
        path: impl AsRef<Path>,
        partition_by_tenant: bool,
    ) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut conn = Connection::open(&path).map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| e.to_string())?;
        // CORR PART 2: drive schema creation through the
        // identifier-based migration framework so the
        // `audit_partition` schema is tracked in
        // `_relix_migrations`. Re-boots are O(log n) lookups
        // against the framework, never CREATE TABLE retries.
        // Legacy claim makes a node that upgrades from the
        // pre-fix path stamp v1 without re-running the CREATE.
        let claim_sql = "audit_partition.v1";
        crate::db::claim_legacy_migration(&conn, "audit_partition.v1", |c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='table' AND name='audit_partition'",
                [],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
        .map_err(|e| e.to_string())?;
        let body_sql = "CREATE TABLE IF NOT EXISTS audit_partition (\n                tenant_id        TEXT    NOT NULL,\n                ts_secs          INTEGER NOT NULL,\n                request_id       TEXT    NOT NULL,\n                caller_name      TEXT    NOT NULL,\n                method           TEXT    NOT NULL,\n                policy_decision  TEXT    NOT NULL,\n                status           TEXT    NOT NULL,\n                error_kind       INTEGER,\n                latency_ms       INTEGER NOT NULL,\n                PRIMARY KEY (tenant_id, ts_secs, request_id)\n            );\n            CREATE INDEX IF NOT EXISTS idx_audit_partition_ts\n                ON audit_partition(tenant_id, ts_secs DESC);";
        crate::db::apply_migration(&mut conn, "audit_partition.v1", body_sql, |tx| {
            tx.execute_batch(body_sql)
        })
        .map_err(|e| e.to_string())?;
        let _ = claim_sql;
        Ok(Self {
            path,
            conn: Mutex::new(conn),
            partition_by_tenant,
        })
    }

    /// `true` when this store was opened in fail-closed
    /// (per-tenant) mode.
    pub fn partition_by_tenant(&self) -> bool {
        self.partition_by_tenant
    }

    /// Append one row. The bridge calls this just before
    /// finalising the canonical CBOR record.
    pub fn append(&self, row: &PartitionRow) -> Result<(), String> {
        // PART 4: fail-closed on missing tenant when
        // partition-by-tenant mode is on.
        if self.partition_by_tenant
            && row
                .tenant_id
                .as_deref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        {
            return Err(
                "audit_partition: tenant_id required when partition_by_tenant = true".to_string(),
            );
        }
        let tenant = sanitise_tenant_id(row.tenant_id.as_deref());
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT OR REPLACE INTO audit_partition
             (tenant_id, ts_secs, request_id, caller_name, method,
              policy_decision, status, error_kind, latency_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                tenant,
                row.ts_secs,
                row.request_id_hex,
                row.caller_name,
                row.method,
                row.policy_decision,
                row.status,
                row.error_kind,
                row.latency_ms as i64,
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Enumerate every distinct tenant id seen by the mirror.
    /// Sorted ascending; deterministic for tests.
    pub fn list_tenants(&self) -> Result<Vec<String>, String> {
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare("SELECT DISTINCT tenant_id FROM audit_partition ORDER BY tenant_id ASC")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    /// Read the most recent rows for `tenant_id`. `limit` is
    /// clamped to `[1, 1000]` server-side. Newest first.
    pub fn tenant_recent(
        &self,
        tenant_id: &str,
        limit: usize,
    ) -> Result<Vec<PartitionReadRow>, String> {
        let tenant = sanitise_tenant_id(Some(tenant_id));
        let cap = limit.clamp(1, 1000) as i64;
        let conn = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = conn
            .prepare(
                "SELECT ts_secs, request_id, tenant_id, caller_name, method,
                        policy_decision, status, error_kind, latency_ms
                 FROM audit_partition
                 WHERE tenant_id = ?1
                 ORDER BY ts_secs DESC, request_id DESC
                 LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![tenant, cap], |r| {
                Ok(PartitionReadRow {
                    ts_secs: r.get(0)?,
                    request_id: r.get(1)?,
                    tenant_id: r.get(2)?,
                    caller_name: r.get(3)?,
                    method: r.get(4)?,
                    policy_decision: r.get(5)?,
                    status: r.get(6)?,
                    error_kind: r.get(7)?,
                    latency_ms: r.get::<_, i64>(8)? as u64,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    /// Path on disk (diagnostics + tests).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Sanitise a tenant id for the partition key. Same rule used
/// by the Qdrant collection sanitiser + the per-tenant policy
/// path lookup: ASCII alnum + `_`; everything else → `_`.
/// `None` / empty resolves to `"default"`.
pub fn sanitise_tenant_id(raw: Option<&str>) -> String {
    let s = raw.unwrap_or("default");
    if s.is_empty() {
        return "default".to_string();
    }
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(tenant: Option<&str>, ts: i64, rid: &str, method: &str) -> PartitionRow {
        PartitionRow {
            ts_secs: ts,
            request_id_hex: rid.to_string(),
            tenant_id: tenant.map(str::to_string),
            caller_name: "alice".into(),
            method: method.into(),
            policy_decision: "allow:r".into(),
            status: "ok",
            error_kind: None,
            latency_ms: 5,
        }
    }

    #[test]
    fn open_creates_schema_and_round_trips() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = AuditPartitionStore::open(tmp.path().join("audit.db")).expect("open");
        store
            .append(&row(Some("acme"), 1000, "aa", "ai.chat"))
            .expect("append");
        let recent = store.tenant_recent("acme", 10).expect("recent");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].request_id, "aa");
        assert_eq!(recent[0].method, "ai.chat");
        assert_eq!(recent[0].tenant_id, "acme");
    }

    #[test]
    fn list_tenants_returns_distinct_sorted() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = AuditPartitionStore::open(tmp.path().join("audit.db")).expect("open");
        for (t, rid) in [
            (Some("acme"), "1"),
            (Some("globex"), "2"),
            (Some("acme"), "3"),
            (None, "4"),
        ] {
            store.append(&row(t, 1000, rid, "ai.chat")).expect("append");
        }
        let tenants = store.list_tenants().expect("list");
        assert_eq!(tenants, vec!["acme", "default", "globex"]);
    }

    #[test]
    fn tenant_recent_isolates_buckets_and_orders_newest_first() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = AuditPartitionStore::open(tmp.path().join("audit.db")).expect("open");
        store
            .append(&row(Some("acme"), 100, "a1", "ai.chat"))
            .expect("a1");
        store
            .append(&row(Some("acme"), 300, "a3", "ai.chat"))
            .expect("a3");
        store
            .append(&row(Some("acme"), 200, "a2", "ai.chat"))
            .expect("a2");
        store
            .append(&row(Some("globex"), 999, "g1", "tool.web_fetch"))
            .expect("g1");
        let acme = store.tenant_recent("acme", 100).expect("recent");
        assert_eq!(
            acme.iter()
                .map(|r| r.request_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a3", "a2", "a1"]
        );
        // Globex bucket sees only its own row.
        let globex = store.tenant_recent("globex", 100).expect("recent");
        assert_eq!(globex.len(), 1);
        assert_eq!(globex[0].request_id, "g1");
    }

    #[test]
    fn rows_without_tenant_land_in_default_bucket() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = AuditPartitionStore::open(tmp.path().join("audit.db")).expect("open");
        store
            .append(&row(None, 1000, "x", "ai.chat"))
            .expect("append");
        store
            .append(&row(Some(""), 1001, "y", "ai.chat"))
            .expect("append");
        let recent = store.tenant_recent("default", 10).expect("recent");
        assert_eq!(recent.len(), 2);
    }

    #[test]
    fn tenant_id_sanitised_so_special_chars_collapse() {
        // Both "acme/eu" and "acme_eu" must land in the same
        // sanitised bucket — the slash is rewritten to '_'.
        let tmp = tempfile::tempdir().expect("tmp");
        let store = AuditPartitionStore::open(tmp.path().join("audit.db")).expect("open");
        store
            .append(&row(Some("acme/eu"), 100, "x", "ai.chat"))
            .expect("append");
        store
            .append(&row(Some("acme_eu"), 200, "y", "ai.chat"))
            .expect("append");
        let tenants = store.list_tenants().expect("list");
        assert_eq!(tenants, vec!["acme_eu"]);
        let rows = store.tenant_recent("acme_eu", 10).expect("recent");
        assert_eq!(rows.len(), 2);
    }

    /// PART 4: fail-closed mode rejects rows without a tenant
    /// id rather than silently dropping them into the
    /// `"default"` bucket.
    #[test]
    fn fix_part4_partition_by_tenant_mode_rejects_missing_tenant() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = AuditPartitionStore::open_with_partition(tmp.path().join("audit.db"), true)
            .expect("open");
        // None tenant → error.
        let err = store.append(&row(None, 1, "x", "ai.chat")).unwrap_err();
        assert!(
            err.contains("tenant_id required"),
            "expected fail-closed message, got: {err}"
        );
        // Empty string tenant → error too.
        let err2 = store.append(&row(Some(""), 1, "y", "ai.chat")).unwrap_err();
        assert!(err2.contains("tenant_id required"), "got: {err2}");
        // Whitespace-only also rejected.
        let err3 = store
            .append(&row(Some("   "), 1, "z", "ai.chat"))
            .unwrap_err();
        assert!(err3.contains("tenant_id required"), "got: {err3}");
        // A valid tenant id succeeds.
        store
            .append(&row(Some("acme"), 1, "ok", "ai.chat"))
            .expect("valid tenant accepted");
    }

    /// PART 4: legacy (non-partitioning) mode keeps the
    /// `"default"` fall-through so single-tenant deployments
    /// stay byte-identical.
    #[test]
    fn fix_part4_legacy_mode_accepts_missing_tenant_into_default() {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = AuditPartitionStore::open(tmp.path().join("audit.db")).expect("open");
        assert!(!store.partition_by_tenant());
        store
            .append(&row(None, 1, "x", "ai.chat"))
            .expect("none tenant accepted in legacy mode");
        let rows = store.tenant_recent("default", 10).expect("recent");
        assert_eq!(rows.len(), 1);
    }
}
