//! SQLite-backed delivery-state store for the §7.30 PART 1
//! out-of-band approval pipeline. Holds one row per approval
//! request with the wire-friendly columns the spec mandates.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// One row in `approval_delivery`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalDeliveryRow {
    pub approval_id: String,
    pub agent_name: String,
    pub capability: String,
    pub request_summary: String,
    pub session_id: String,
    /// `pending` | `approved` | `rejected` | `expired`.
    pub status: String,
    pub delivery_channel: String,
    pub escalated: bool,
    pub escalation_channel: Option<String>,
    pub delivered_at_ms: Option<i64>,
    pub escalated_at_ms: Option<i64>,
    pub decided_at_ms: Option<i64>,
    pub decision: Option<String>,
    pub decision_note: Option<String>,
}

#[derive(Debug, Error)]
pub enum ApprovalStoreError {
    #[error("approval store: sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("approval store: lock poisoned")]
    Lock,
}

/// Cheap-to-clone SQLite-backed store.
#[derive(Clone)]
pub struct ApprovalRequestStore {
    conn: Arc<Mutex<Connection>>,
}

impl ApprovalRequestStore {
    pub fn open(path: &Path) -> Result<Self, ApprovalStoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::log_integrity_warning(&conn, "approval_delivery");
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Self, ApprovalStoreError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn migrate(conn: &Connection) -> Result<(), ApprovalStoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS approval_delivery (\
                 approval_id        TEXT PRIMARY KEY,\
                 agent_name         TEXT NOT NULL,\
                 capability         TEXT NOT NULL,\
                 request_summary    TEXT NOT NULL DEFAULT '',\
                 session_id         TEXT NOT NULL DEFAULT '',\
                 status             TEXT NOT NULL DEFAULT 'pending',\
                 delivery_channel   TEXT NOT NULL DEFAULT '',\
                 escalated          INTEGER NOT NULL DEFAULT 0,\
                 escalation_channel TEXT,\
                 delivered_at_ms    INTEGER,\
                 escalated_at_ms    INTEGER,\
                 decided_at_ms      INTEGER,\
                 decision           TEXT,\
                 decision_note      TEXT\
             );\
             CREATE INDEX IF NOT EXISTS approval_delivery_status_idx \
                 ON approval_delivery(status);\
             CREATE INDEX IF NOT EXISTS approval_delivery_agent_idx \
                 ON approval_delivery(agent_name);",
        )?;
        // RELIX-7.30 PART 1: column_exists-guarded ALTERs so a
        // pre-7.30 database (none exist today, but the same
        // pattern is the spec's standard) picks the new
        // columns up on open. Idempotent on a fresh schema.
        Self::ensure_column(conn, "delivery_channel", "TEXT")?;
        Self::ensure_column(conn, "escalated", "INTEGER NOT NULL DEFAULT 0")?;
        Self::ensure_column(conn, "escalation_channel", "TEXT")?;
        Self::ensure_column(conn, "delivered_at_ms", "INTEGER")?;
        Self::ensure_column(conn, "escalated_at_ms", "INTEGER")?;
        Ok(())
    }

    fn ensure_column(
        conn: &Connection,
        column: &str,
        column_decl: &str,
    ) -> Result<(), ApprovalStoreError> {
        let mut stmt = conn.prepare("PRAGMA table_info(approval_delivery)")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(());
            }
        }
        drop(rows);
        drop(stmt);
        let sql = format!("ALTER TABLE approval_delivery ADD COLUMN {column} {column_decl}");
        conn.execute(&sql, [])?;
        Ok(())
    }

    /// Insert OR replace the row keyed by `approval_id`.
    pub fn upsert(&self, row: &ApprovalDeliveryRow) -> Result<(), ApprovalStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR REPLACE INTO approval_delivery \
             (approval_id, agent_name, capability, request_summary, session_id, status, \
              delivery_channel, escalated, escalation_channel, delivered_at_ms, escalated_at_ms, \
              decided_at_ms, decision, decision_note) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                row.approval_id,
                row.agent_name,
                row.capability,
                row.request_summary,
                row.session_id,
                row.status,
                row.delivery_channel,
                row.escalated as i32,
                row.escalation_channel,
                row.delivered_at_ms,
                row.escalated_at_ms,
                row.decided_at_ms,
                row.decision,
                row.decision_note,
            ],
        )?;
        Ok(())
    }

    pub fn get(
        &self,
        approval_id: &str,
    ) -> Result<Option<ApprovalDeliveryRow>, ApprovalStoreError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT approval_id, agent_name, capability, request_summary, session_id, status, \
                    delivery_channel, escalated, escalation_channel, delivered_at_ms, \
                    escalated_at_ms, decided_at_ms, decision, decision_note \
             FROM approval_delivery WHERE approval_id = ?1",
            params![approval_id],
            row_to_record,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list(
        &self,
        status_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ApprovalDeliveryRow>, ApprovalStoreError> {
        let conn = self.lock()?;
        let limit_i = limit.clamp(1, 5000) as i64;
        let mut stmt = if status_filter.is_some() {
            conn.prepare(
                "SELECT approval_id, agent_name, capability, request_summary, session_id, status, \
                        delivery_channel, escalated, escalation_channel, delivered_at_ms, \
                        escalated_at_ms, decided_at_ms, decision, decision_note \
                 FROM approval_delivery WHERE status = ?1 \
                 ORDER BY delivered_at_ms DESC, approval_id ASC LIMIT ?2",
            )?
        } else {
            conn.prepare(
                "SELECT approval_id, agent_name, capability, request_summary, session_id, status, \
                        delivery_channel, escalated, escalation_channel, delivered_at_ms, \
                        escalated_at_ms, decided_at_ms, decision, decision_note \
                 FROM approval_delivery \
                 ORDER BY delivered_at_ms DESC, approval_id ASC LIMIT ?1",
            )?
        };
        let rows: Vec<ApprovalDeliveryRow> = if let Some(s) = status_filter {
            stmt.query_map(params![s, limit_i], row_to_record)?
                .collect::<Result<_, _>>()?
        } else {
            stmt.query_map(params![limit_i], row_to_record)?
                .collect::<Result<_, _>>()?
        };
        Ok(rows)
    }

    pub fn mark_escalated(
        &self,
        approval_id: &str,
        escalation_channel: &str,
        escalated_at_ms: i64,
    ) -> Result<(), ApprovalStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE approval_delivery \
             SET escalated = 1, escalation_channel = ?1, escalated_at_ms = ?2 \
             WHERE approval_id = ?3 AND status = 'pending'",
            params![escalation_channel, escalated_at_ms, approval_id],
        )?;
        Ok(())
    }

    pub fn record_decision(
        &self,
        approval_id: &str,
        decision: &str,
        note: Option<&str>,
        decided_at_ms: i64,
    ) -> Result<(), ApprovalStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "UPDATE approval_delivery \
             SET status = ?1, decision = ?1, decision_note = ?2, decided_at_ms = ?3 \
             WHERE approval_id = ?4",
            params![decision, note, decided_at_ms, approval_id],
        )?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, ApprovalStoreError> {
        self.conn.lock().map_err(|_| ApprovalStoreError::Lock)
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalDeliveryRow> {
    Ok(ApprovalDeliveryRow {
        approval_id: row.get(0)?,
        agent_name: row.get(1)?,
        capability: row.get(2)?,
        request_summary: row.get(3)?,
        session_id: row.get(4)?,
        status: row.get(5)?,
        delivery_channel: row.get(6)?,
        escalated: row.get::<_, i64>(7)? != 0,
        escalation_channel: row.get(8)?,
        delivered_at_ms: row.get(9)?,
        escalated_at_ms: row.get(10)?,
        decided_at_ms: row.get(11)?,
        decision: row.get(12)?,
        decision_note: row.get(13)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_row(id: &str) -> ApprovalDeliveryRow {
        ApprovalDeliveryRow {
            approval_id: id.into(),
            agent_name: "alice".into(),
            capability: "tool.fs.write".into(),
            request_summary: "writes a sensitive file".into(),
            session_id: "sess1".into(),
            status: "pending".into(),
            delivery_channel: "telegram".into(),
            escalated: false,
            escalation_channel: Some("slack".into()),
            delivered_at_ms: Some(1_000),
            escalated_at_ms: None,
            decided_at_ms: None,
            decision: None,
            decision_note: None,
        }
    }

    #[test]
    fn open_in_memory_creates_schema_and_indexes() {
        let s = ApprovalRequestStore::open_in_memory().unwrap();
        let all = s.list(None, 10).unwrap();
        assert!(all.is_empty());
    }

    #[test]
    fn upsert_then_get_round_trips() {
        let s = ApprovalRequestStore::open_in_memory().unwrap();
        let r = fixture_row("a1");
        s.upsert(&r).unwrap();
        let got = s.get("a1").unwrap().unwrap();
        assert_eq!(got, r);
    }

    #[test]
    fn mark_escalated_only_updates_pending_rows() {
        let s = ApprovalRequestStore::open_in_memory().unwrap();
        let r = fixture_row("a2");
        s.upsert(&r).unwrap();
        s.mark_escalated("a2", "email", 2_000).unwrap();
        let row = s.get("a2").unwrap().unwrap();
        assert!(row.escalated);
        assert_eq!(row.escalation_channel.as_deref(), Some("email"));
        assert_eq!(row.escalated_at_ms, Some(2_000));

        // Decide → escalation no longer mutates.
        s.record_decision("a2", "approved", Some("ok"), 3_000)
            .unwrap();
        s.mark_escalated("a2", "dashboard", 4_000).unwrap();
        let row2 = s.get("a2").unwrap().unwrap();
        // Decision stuck; escalation channel from earlier remains.
        assert_eq!(row2.status, "approved");
        assert_eq!(row2.escalation_channel.as_deref(), Some("email"));
    }

    #[test]
    fn record_decision_updates_status_and_note() {
        let s = ApprovalRequestStore::open_in_memory().unwrap();
        s.upsert(&fixture_row("a3")).unwrap();
        s.record_decision("a3", "rejected", Some("nope"), 9_000)
            .unwrap();
        let row = s.get("a3").unwrap().unwrap();
        assert_eq!(row.status, "rejected");
        assert_eq!(row.decision.as_deref(), Some("rejected"));
        assert_eq!(row.decision_note.as_deref(), Some("nope"));
        assert_eq!(row.decided_at_ms, Some(9_000));
    }

    #[test]
    fn list_filters_by_status_and_orders_newest_first() {
        let s = ApprovalRequestStore::open_in_memory().unwrap();
        let mut r1 = fixture_row("a1");
        r1.delivered_at_ms = Some(100);
        let mut r2 = fixture_row("a2");
        r2.delivered_at_ms = Some(200);
        s.upsert(&r1).unwrap();
        s.upsert(&r2).unwrap();
        let pending = s.list(Some("pending"), 10).unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].approval_id, "a2");
        assert_eq!(pending[1].approval_id, "a1");
        s.record_decision("a1", "approved", None, 300).unwrap();
        let approved = s.list(Some("approved"), 10).unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].approval_id, "a1");
    }
}
