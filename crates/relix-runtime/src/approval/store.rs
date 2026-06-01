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
    /// Stable identifier — used by the operator's response path
    /// to route the decision back to the right pending row.
    pub approval_id: String,
    /// Friendly name of the agent that asked for approval.
    pub agent_name: String,
    /// Capability / method the agent asked to invoke.
    pub capability: String,
    /// Operator-readable summary of the request.
    pub request_summary: String,
    /// Originating session id.
    pub session_id: String,
    /// `pending` | `approved` | `rejected` | `expired` |
    /// `delivery_failed`. The last state is set by
    /// `ApprovalDeliveryService::dispatch_request` when the
    /// per-channel send returns an error so operators can
    /// reconcile failed notifications via
    /// `GET /v1/approval/failed-deliveries`.
    pub status: String,
    /// Wire-string tag of the channel resolved for the initial
    /// delivery (`telegram`, `slack`, `discord`, `email`,
    /// `dashboard`).
    pub delivery_channel: String,
    /// `true` once the escalation timer has fired and the
    /// escalation channel has been notified.
    pub escalated: bool,
    /// Wire-string tag of the escalation channel — set on the
    /// row at dispatch time so it survives a controller
    /// restart between the initial delivery and the timer
    /// firing.
    pub escalation_channel: Option<String>,
    /// Set AFTER the initial dispatcher returns Ok. `None`
    /// while the row is still queued / mid-send and on rows
    /// that landed in `delivery_failed`.
    pub delivered_at_ms: Option<i64>,
    /// Set once the escalation timer fires and the escalation
    /// channel has been notified.
    pub escalated_at_ms: Option<i64>,
    /// Set when the operator records a decision (or the
    /// expiry sweep marks the row expired).
    pub decided_at_ms: Option<i64>,
    /// `approved` | `rejected` | `expired` — `None` while the
    /// row is still pending.
    pub decision: Option<String>,
    /// Free-form note the operator may attach to their
    /// decision.
    pub decision_note: Option<String>,
    /// Error message from the most-recent failed dispatcher
    /// call. Only set when `status = "delivery_failed"`; the
    /// schema column carries the message so operators can
    /// triage without grepping log files.
    pub delivery_error: Option<String>,
    /// SEC PART B: explicit authorised approver allow-list
    /// (subject id hex). Empty ⇒ caps fall back to
    /// role-based admission (`operator` / `admin`). Stored as
    /// a JSON-encoded string column.
    #[serde(default)]
    pub authorized_approvers: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ApprovalStoreError {
    #[error("approval store: sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("approval store: lock poisoned")]
    Lock,
    /// SEC PART B: the `record_decision` write found the row
    /// is no longer pending. Returned instead of a silent
    /// no-op so callers can surface "already decided" to the
    /// operator.
    #[error("approval store: approval `{0}` is not pending (already decided)")]
    AlreadyDecided(String),
    /// SEC PART B: an internal JSON encode / decode failure on
    /// the `authorized_approvers` column. Should never reach
    /// production — the column is round-tripped through
    /// `serde_json` with a well-formed Vec source.
    #[error("approval store: authorized_approvers encode/decode: {0}")]
    Json(String),
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
                 decision_note      TEXT,\
                 delivery_error     TEXT,\
                 authorized_approvers TEXT NOT NULL DEFAULT '[]'\
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
        // PART 6: `delivery_error` carries the most-recent failed
        // dispatcher message so operators can triage without
        // grepping log files. Nullable — only populated when
        // `status = "delivery_failed"`.
        Self::ensure_column(conn, "delivery_error", "TEXT")?;
        // SEC PART B: `authorized_approvers` carries the
        // JSON-encoded allow-list of subject ids that may
        // record a decision on the row. NOT NULL with default
        // `'[]'` so legacy rows back-fill cleanly.
        Self::ensure_column(conn, "authorized_approvers", "TEXT NOT NULL DEFAULT '[]'")?;
        // GROUP 6: tenant isolation column (idempotent).
        crate::db::ensure_tenant_id_column(conn, "approval_delivery")?;
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

    /// Insert OR replace the row keyed by `approval_id`. New
    /// rows from `dispatch_request` go in with `status =
    /// "pending"` and `delivered_at_ms = None`; the timestamp
    /// is stamped via [`Self::mark_delivered`] AFTER the
    /// per-channel send actually succeeds.
    /// GROUP 6: tenant-blind upsert — writes the reserved
    /// `'default'` tenant. Retained for existing call sites; new
    /// code should prefer [`Self::upsert_for_tenant`].
    pub fn upsert(&self, row: &ApprovalDeliveryRow) -> Result<(), ApprovalStoreError> {
        self.upsert_for_tenant(row, "default")
    }

    /// GROUP 6: upsert attributed to the caller's VERIFIED tenant.
    pub fn upsert_for_tenant(
        &self,
        row: &ApprovalDeliveryRow,
        tenant_id: &str,
    ) -> Result<(), ApprovalStoreError> {
        let tenant = if tenant_id.trim().is_empty() {
            "default"
        } else {
            tenant_id
        };
        let conn = self.lock()?;
        let approvers_json = serde_json::to_string(&row.authorized_approvers)
            .map_err(|e| ApprovalStoreError::Json(e.to_string()))?;
        conn.execute(
            "INSERT OR REPLACE INTO approval_delivery \
             (approval_id, agent_name, capability, request_summary, session_id, status, \
              delivery_channel, escalated, escalation_channel, delivered_at_ms, escalated_at_ms, \
              decided_at_ms, decision, decision_note, delivery_error, authorized_approvers, tenant_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
                row.delivery_error,
                approvers_json,
                tenant,
            ],
        )?;
        Ok(())
    }

    /// GROUP 6: tenant-scoped lookup — returns the row ONLY when
    /// it belongs to `tenant`, so a caller scoped to tenant A
    /// cannot read tenant B's approval-delivery row by id.
    pub fn get_for_tenant(
        &self,
        approval_id: &str,
        tenant: &str,
    ) -> Result<Option<ApprovalDeliveryRow>, ApprovalStoreError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT approval_id, agent_name, capability, request_summary, session_id, status, \
                    delivery_channel, escalated, escalation_channel, delivered_at_ms, \
                    escalated_at_ms, decided_at_ms, decision, decision_note, delivery_error, \
                    authorized_approvers \
             FROM approval_delivery WHERE approval_id = ?1 AND tenant_id = ?2",
            params![approval_id, tenant],
            row_to_record,
        )
        .optional()
        .map_err(Into::into)
    }

    /// PART 6: stamp `delivered_at_ms` AFTER the per-channel
    /// send returned Ok. Only updates rows whose status is
    /// still `pending` so a later decision (or an
    /// already-failed delivery) does not get its delivery
    /// timestamp re-written.
    pub fn mark_delivered(
        &self,
        approval_id: &str,
        delivered_at_ms: i64,
    ) -> Result<bool, ApprovalStoreError> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE approval_delivery \
             SET delivered_at_ms = ?1, delivery_error = NULL \
             WHERE approval_id = ?2 AND status = 'pending'",
            params![delivered_at_ms, approval_id],
        )?;
        Ok(changed > 0)
    }

    /// PART 6: flip the row to `delivery_failed` when the
    /// per-channel send returns Err. Stamps the error message
    /// + the timestamp the failure was observed.
    pub fn mark_delivery_failed(
        &self,
        approval_id: &str,
        error: &str,
        failed_at_ms: i64,
    ) -> Result<bool, ApprovalStoreError> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE approval_delivery \
             SET status = 'delivery_failed', delivery_error = ?1, decided_at_ms = ?2 \
             WHERE approval_id = ?3 AND status = 'pending'",
            params![error, failed_at_ms, approval_id],
        )?;
        Ok(changed > 0)
    }

    /// Fetch one row by id.
    pub fn get(
        &self,
        approval_id: &str,
    ) -> Result<Option<ApprovalDeliveryRow>, ApprovalStoreError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT approval_id, agent_name, capability, request_summary, session_id, status, \
                    delivery_channel, escalated, escalation_channel, delivered_at_ms, \
                    escalated_at_ms, decided_at_ms, decision, decision_note, delivery_error, \
                    authorized_approvers \
             FROM approval_delivery WHERE approval_id = ?1",
            params![approval_id],
            row_to_record,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Return all rows matching `status_filter` (or every row
    /// when `None`), newest-first. `limit` is clamped to
    /// `[1, 5000]`.
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
                        escalated_at_ms, decided_at_ms, decision, decision_note, delivery_error, \
                        authorized_approvers \
                 FROM approval_delivery WHERE status = ?1 \
                 ORDER BY COALESCE(delivered_at_ms, decided_at_ms, 0) DESC, approval_id ASC \
                 LIMIT ?2",
            )?
        } else {
            conn.prepare(
                "SELECT approval_id, agent_name, capability, request_summary, session_id, status, \
                        delivery_channel, escalated, escalation_channel, delivered_at_ms, \
                        escalated_at_ms, decided_at_ms, decision, decision_note, delivery_error, \
                        authorized_approvers \
                 FROM approval_delivery \
                 ORDER BY COALESCE(delivered_at_ms, decided_at_ms, 0) DESC, approval_id ASC \
                 LIMIT ?1",
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

    /// PART 6: shortcut around `list` for the failed-deliveries
    /// reconciliation endpoint. Equivalent to
    /// `list(Some("delivery_failed"), limit)` but documents
    /// the call-site intent.
    pub fn list_failed_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<ApprovalDeliveryRow>, ApprovalStoreError> {
        self.list(Some("delivery_failed"), limit)
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

    /// SEC PART B: flip a `pending` row to a terminal
    /// `approved` / `rejected` / `expired` state.
    ///
    /// The `WHERE status = 'pending'` guard prevents a decided
    /// row from being re-decided silently. When 0 rows are
    /// affected we return [`ApprovalStoreError::AlreadyDecided`]
    /// so the cap layer can surface "already decided" to the
    /// operator instead of pretending the write succeeded.
    pub fn record_decision(
        &self,
        approval_id: &str,
        decision: &str,
        note: Option<&str>,
        decided_at_ms: i64,
    ) -> Result<(), ApprovalStoreError> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE approval_delivery \
             SET status = ?1, decision = ?1, decision_note = ?2, decided_at_ms = ?3 \
             WHERE approval_id = ?4 AND status = 'pending'",
            params![decision, note, decided_at_ms, approval_id],
        )?;
        if changed == 0 {
            return Err(ApprovalStoreError::AlreadyDecided(approval_id.to_string()));
        }
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, ApprovalStoreError> {
        self.conn.lock().map_err(|_| ApprovalStoreError::Lock)
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalDeliveryRow> {
    let approvers_json: String = row.get(15)?;
    let authorized_approvers: Vec<String> =
        serde_json::from_str(&approvers_json).unwrap_or_default();
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
        delivery_error: row.get(14)?,
        authorized_approvers,
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
            delivery_error: None,
            authorized_approvers: Vec::new(),
        }
    }

    #[test]
    fn group6_approval_delivery_reads_are_isolated_by_verified_tenant() {
        // Two tenants record approval-delivery rows. A
        // tenant-scoped get must return ONLY the caller's
        // tenant's row — never the other tenant's, by id.
        let s = ApprovalRequestStore::open_in_memory().unwrap();
        s.upsert_for_tenant(&fixture_row("ap-a"), "tenant-a")
            .unwrap();
        s.upsert_for_tenant(&fixture_row("ap-b"), "tenant-b")
            .unwrap();
        assert!(s.get_for_tenant("ap-a", "tenant-a").unwrap().is_some());
        assert!(
            s.get_for_tenant("ap-b", "tenant-a").unwrap().is_none(),
            "tenant A must not read tenant B's approval-delivery row"
        );
        assert!(s.get_for_tenant("ap-b", "tenant-b").unwrap().is_some());
    }

    /// PART 6 fixture: row in the "pending, not yet
    /// delivered" state that mirrors what
    /// `dispatch_request` inserts before calling
    /// `dispatch.send()`.
    fn pending_row(id: &str) -> ApprovalDeliveryRow {
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
            delivered_at_ms: None,
            escalated_at_ms: None,
            decided_at_ms: None,
            decision: None,
            decision_note: None,
            delivery_error: None,
            authorized_approvers: Vec::new(),
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
    fn record_decision_refuses_to_redecide_a_decided_row() {
        // SEC PART B: the UPDATE has `WHERE status = 'pending'`
        // so a re-decide cannot silently overwrite. The cap
        // surfaces this as INVALID_ARGS so operators see
        // "already decided" instead of a phantom success.
        let s = ApprovalRequestStore::open_in_memory().unwrap();
        let mut r = fixture_row("a-redec");
        r.status = "pending".into();
        r.decision = None;
        r.decision_note = None;
        r.decided_at_ms = None;
        s.upsert(&r).unwrap();
        s.record_decision("a-redec", "approved", Some("first"), 1_000)
            .unwrap();
        match s.record_decision("a-redec", "rejected", Some("second"), 2_000) {
            Err(ApprovalStoreError::AlreadyDecided(id)) => assert_eq!(id, "a-redec"),
            other => panic!("expected AlreadyDecided, got {other:?}"),
        }
        // First decision is preserved.
        let row = s.get("a-redec").unwrap().unwrap();
        assert_eq!(row.status, "approved");
        assert_eq!(row.decision_note.as_deref(), Some("first"));
        assert_eq!(row.decided_at_ms, Some(1_000));
    }

    #[test]
    fn authorized_approvers_round_trip_through_store() {
        // SEC PART B: the JSON-encoded approver list is the
        // proof the dispatch cap consults. Round-trip via
        // upsert → get → list to lock the column.
        let s = ApprovalRequestStore::open_in_memory().unwrap();
        let mut r = fixture_row("a-approvers");
        r.authorized_approvers = vec!["subj-alice".into(), "subj-bob".into()];
        s.upsert(&r).unwrap();
        let row = s.get("a-approvers").unwrap().unwrap();
        assert_eq!(
            row.authorized_approvers,
            vec!["subj-alice".to_string(), "subj-bob".to_string()]
        );
        let listed = s.list(Some("pending"), 50).unwrap();
        let found = listed
            .iter()
            .find(|r| r.approval_id == "a-approvers")
            .expect("row must be listed");
        assert_eq!(
            found.authorized_approvers,
            vec!["subj-alice".to_string(), "subj-bob".to_string()]
        );
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

    // ── PART 6 — delivery-status ordering ─────────────────

    #[test]
    fn pending_row_round_trips_with_null_delivered_at_ms() {
        let s = ApprovalRequestStore::open_in_memory().unwrap();
        let r = pending_row("a1");
        s.upsert(&r).unwrap();
        let got = s.get("a1").unwrap().unwrap();
        assert_eq!(got, r);
        assert!(got.delivered_at_ms.is_none());
        assert!(got.delivery_error.is_none());
    }

    #[test]
    fn mark_delivered_stamps_timestamp_on_pending_row() {
        let s = ApprovalRequestStore::open_in_memory().unwrap();
        let r = pending_row("a1");
        s.upsert(&r).unwrap();
        let changed = s.mark_delivered("a1", 12_345).unwrap();
        assert!(changed);
        let row = s.get("a1").unwrap().unwrap();
        assert_eq!(row.delivered_at_ms, Some(12_345));
        assert!(row.delivery_error.is_none());
        assert_eq!(row.status, "pending");
    }

    #[test]
    fn mark_delivered_does_not_touch_decided_rows() {
        let s = ApprovalRequestStore::open_in_memory().unwrap();
        let r = pending_row("a1");
        s.upsert(&r).unwrap();
        s.mark_delivered("a1", 100).unwrap();
        s.record_decision("a1", "approved", None, 200).unwrap();
        // Try to re-mark — the row is no longer pending so the
        // delivered_at_ms timestamp must NOT regress.
        let changed = s.mark_delivered("a1", 999).unwrap();
        assert!(!changed);
        let row = s.get("a1").unwrap().unwrap();
        assert_eq!(row.delivered_at_ms, Some(100));
        assert_eq!(row.status, "approved");
    }

    #[test]
    fn mark_delivery_failed_flips_status_and_records_error() {
        let s = ApprovalRequestStore::open_in_memory().unwrap();
        let r = pending_row("a1");
        s.upsert(&r).unwrap();
        let changed = s
            .mark_delivery_failed("a1", "telegram: HTTP 502", 9_999)
            .unwrap();
        assert!(changed);
        let row = s.get("a1").unwrap().unwrap();
        assert_eq!(row.status, "delivery_failed");
        assert_eq!(row.delivery_error.as_deref(), Some("telegram: HTTP 502"));
        assert_eq!(row.decided_at_ms, Some(9_999));
        // delivered_at_ms STAYS None — a failed send did not
        // produce a delivery, so any later reconciliation
        // can distinguish "sent" from "tried and failed".
        assert!(row.delivered_at_ms.is_none());
    }

    #[test]
    fn mark_delivery_failed_does_not_overwrite_decided_rows() {
        let s = ApprovalRequestStore::open_in_memory().unwrap();
        let r = pending_row("a1");
        s.upsert(&r).unwrap();
        s.mark_delivered("a1", 100).unwrap();
        s.record_decision("a1", "approved", None, 200).unwrap();
        let changed = s.mark_delivery_failed("a1", "spurious", 300).unwrap();
        assert!(!changed);
        let row = s.get("a1").unwrap().unwrap();
        assert_eq!(row.status, "approved");
        assert!(row.delivery_error.is_none());
    }

    #[test]
    fn list_failed_deliveries_returns_only_failed_rows() {
        let s = ApprovalRequestStore::open_in_memory().unwrap();
        s.upsert(&pending_row("a1")).unwrap();
        s.upsert(&pending_row("a2")).unwrap();
        s.upsert(&pending_row("a3")).unwrap();
        s.mark_delivered("a1", 100).unwrap();
        s.mark_delivery_failed("a2", "telegram: 502", 200).unwrap();
        s.mark_delivery_failed("a3", "slack: not_in_channel", 300)
            .unwrap();
        let failed = s.list_failed_deliveries(10).unwrap();
        assert_eq!(failed.len(), 2);
        // Newest-first ordering — a3's decided_at_ms (300) is
        // newer than a2's (200).
        assert_eq!(failed[0].approval_id, "a3");
        assert_eq!(
            failed[0].delivery_error.as_deref(),
            Some("slack: not_in_channel")
        );
        assert_eq!(failed[1].approval_id, "a2");
    }
}
