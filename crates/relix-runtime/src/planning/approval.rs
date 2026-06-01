//! RELIX-7.24 Stage-4 — human-in-the-loop spec approval gate.
//!
//! When `planning.create_plan` is called with
//! `require_approval = true`, the coordinator runs the full
//! parse → orchestrate → resolve → critic pipeline but stops
//! BEFORE execution. The fully-decorated plan is written into
//! a SQLite-backed [`ApprovalStore`] in the `pending` state
//! and an [`ApprovalNotifier`] fans out one notification per
//! configured channel target (Telegram / Discord / Slack /
//! email — same `*.send` capability surface the alert
//! delivery system uses).
//!
//! The operator then decides via one of three paths:
//!
//! - `planning.approve_plan { plan_id, note? }` —
//!   sets `status = approved`, verifies the spec signature has
//!   not been tampered with since the plan was persisted, and
//!   then executes the stored workflow through the existing
//!   `WorkflowDispatcher`.
//! - `planning.reject_plan { plan_id, note? }` — sets
//!   `status = rejected`; no execution happens.
//! - `planning.list_approvals` / `planning.get_approval` —
//!   read-only views the bridge proxies for the dashboard +
//!   CLI.
//!
//! A background expiry task (spawned by the controller
//! runtime) sweeps every 60 seconds and rejects any plan
//! whose `(created_at_ms + approval_timeout_secs * 1000) <
//! now_ms`. The rejection is written with
//! `decision_note = "expired after Ns"` so operators can see
//! when a plan auto-expired vs. was actively rejected.
//!
//! ## Persistence
//!
//! `plan_approvals.sqlite` lives at the configured
//! `[planning] approval_db_path`. Schema:
//!
//! ```sql
//! CREATE TABLE plan_approvals (
//!     id              TEXT PRIMARY KEY,   -- spec_id (uuid v4)
//!     spec_json       TEXT NOT NULL,      -- full PlanSpec JSON
//!     workflow_yaml   TEXT NOT NULL,      -- generated workflow YAML
//!     status          TEXT NOT NULL,      -- pending|approved|rejected|expired
//!     created_at_ms   INTEGER NOT NULL,
//!     decided_at_ms   INTEGER,
//!     decision_note   TEXT,
//!     orchestrator_meta TEXT,             -- JSON; null when not active
//!     critic_meta     TEXT                -- JSON; null when skipped
//! );
//! CREATE INDEX plan_approvals_status_idx ON plan_approvals(status);
//! ```
//!
//! Every connection runs the standard relix pragmas
//! (`foreign_keys = ON`, `journal_mode = WAL`,
//! `synchronous = NORMAL`, `busy_timeout = 5000`) via
//! [`crate::db::apply_pragmas`].

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::PlanSpec;

/// Default approval timeout in seconds. Operators override
/// via `[planning] approval_timeout_secs`. One hour matches
/// the spec's documented default.
pub const DEFAULT_APPROVAL_TIMEOUT_SECS: i64 = 3600;

/// Status of one approval record. Wire-serialised as
/// lowercase snake_case to match the bridge response shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
        }
    }

    /// Parse a status from a wire string. Returns `None` on
    /// any unknown value so the caller can surface a useful
    /// INVALID_ARGS error.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "approved" => Some(Self::Approved),
            "rejected" => Some(Self::Rejected),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }

    /// Decision statuses — the operator-driven set. `Pending`
    /// and `Expired` are NOT decisions (the latter is an
    /// automatic state change).
    pub fn is_decision(self) -> bool {
        matches!(self, Self::Approved | Self::Rejected)
    }
}

/// One row in [`ApprovalStore`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalRecord {
    /// Same as the underlying [`PlanSpec::spec_id`].
    pub plan_id: String,
    /// Full spec including its signature so callers can
    /// re-verify before acting on the approval.
    pub spec: PlanSpec,
    /// Generated workflow YAML — the exact bytes the
    /// executor will run when the approval is granted.
    pub workflow_yaml: String,
    pub status: ApprovalStatus,
    pub created_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_at_ms: Option<i64>,
    /// Operator-supplied free-form note on the
    /// approval/rejection decision. Auto-populated to a
    /// specific format by the expiry sweep.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_note: Option<String>,
    /// Orchestrator response fields (sub_goals,
    /// specialist_assignments, decomposed_by_heuristic, etc.)
    /// captured at submission time. `Null` when the
    /// orchestrator was skipped.
    #[serde(default)]
    pub orchestrator_meta: serde_json::Value,
    /// Critic response fields (rounds, approved, warning,
    /// history) captured at submission time. `Null` when the
    /// critic was skipped (dry-run / disabled).
    #[serde(default)]
    pub critic_meta: serde_json::Value,
}

/// Errors surfaced by the approval store + decide flow.
#[derive(Debug, Error)]
pub enum ApprovalError {
    #[error("approval store: sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("approval store: JSON encode/decode: {0}")]
    Json(#[from] serde_json::Error),
    #[error("approval store: plan `{0}` not found")]
    NotFound(String),
    #[error("approval store: plan `{plan_id}` is `{status:?}`, not `pending`")]
    NotPending {
        plan_id: String,
        status: ApprovalStatus,
    },
    #[error(
        "approval store: spec signature mismatch on plan `{plan_id}` — \
         the stored spec has been tampered with: {cause}"
    )]
    SpecTampered { plan_id: String, cause: String },

    /// CORR PART 3: returned when [`ApprovalStore::decide`] loses
    /// the race against another concurrent decision on the same
    /// plan. Distinct from [`Self::NotPending`] (which is a stale
    /// read AFTER the transaction observed a non-pending row) —
    /// `AlreadyDecided` fires when the transaction's `UPDATE …
    /// WHERE status = 'pending'` affected zero rows, meaning a
    /// concurrent decide / expire took the row first.
    #[error("approval store: plan `{0}` was already decided by a concurrent caller")]
    AlreadyDecided(String),
}

/// Cheap-to-clone SQLite-backed approval store.
#[derive(Clone)]
pub struct ApprovalStore {
    conn: Arc<Mutex<Connection>>,
    /// PART 9: best-effort decision mirror. When set, `decide`
    /// invokes it AFTER the primary write so the generic
    /// `ApprovalDeliveryService` store can be flipped to the
    /// same decision. `OnceCell` keeps the wiring race-free in
    /// the controller startup path.
    decision_mirror: Arc<tokio::sync::OnceCell<Arc<dyn relix_core::approval::DecisionMirror>>>,
}

impl ApprovalStore {
    /// Open (or create) the store at `path`. Applies the
    /// standard pragmas, runs migrations, runs an integrity
    /// check, and returns the wrapped connection.
    pub fn open(path: &Path) -> Result<Self, ApprovalError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            // `create_dir_all` is idempotent; ignore the
            // success/error and let the connection open fail
            // for a clear error if the path is unreachable.
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::log_integrity_warning(&conn, "planning_approvals");
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            decision_mirror: Arc::new(tokio::sync::OnceCell::new()),
        })
    }

    /// Open an in-memory store. Used by unit tests so the
    /// approval flow can be exercised without disk side
    /// effects.
    pub fn open_in_memory() -> Result<Self, ApprovalError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            decision_mirror: Arc::new(tokio::sync::OnceCell::new()),
        })
    }

    /// PART 9: install a [`DecisionMirror`]. Idempotent; later
    /// calls are silently ignored. Wired by the controller
    /// startup so a `planning.approve_plan` /
    /// `planning.reject_plan` call also flips the matching row
    /// in the generic `approval_delivery` store (when present).
    pub fn install_decision_mirror(&self, mirror: Arc<dyn relix_core::approval::DecisionMirror>) {
        let _ = self.decision_mirror.set(mirror);
    }

    fn migrate(conn: &Connection) -> Result<(), ApprovalError> {
        crate::db::ensure_migration_table(conn)?;
        let current = crate::db::current_migration_version(conn)?;
        if current < 1 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS plan_approvals (\
                     id                TEXT PRIMARY KEY,\
                     spec_json         TEXT NOT NULL,\
                     workflow_yaml     TEXT NOT NULL,\
                     status            TEXT NOT NULL DEFAULT 'pending',\
                     created_at_ms     INTEGER NOT NULL,\
                     decided_at_ms     INTEGER,\
                     decision_note     TEXT,\
                     orchestrator_meta TEXT,\
                     critic_meta       TEXT\
                 );\
                 CREATE INDEX IF NOT EXISTS plan_approvals_status_idx \
                     ON plan_approvals(status);",
            )?;
            crate::db::record_migration_applied(conn, 1)?;
        }
        if current < 2 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS plan_verifications (\
                     id              INTEGER PRIMARY KEY AUTOINCREMENT,\
                     plan_id         TEXT    NOT NULL,\
                     step_id         TEXT    NOT NULL,\
                     criterion       TEXT    NOT NULL,\
                     strategy_used   TEXT    NOT NULL,\
                     passed          INTEGER NOT NULL,\
                     reason          TEXT    NOT NULL,\
                     verified_at_ms  INTEGER NOT NULL\
                 );\
                 CREATE INDEX IF NOT EXISTS plan_verifications_plan_id_idx \
                     ON plan_verifications(plan_id);",
            )?;
            crate::db::record_migration_applied(conn, 2)?;
        }
        // GROUP 6: tenant isolation. This module tracks its schema
        // with the LEGACY integer-version scheme (versions 1, 2
        // above), so — per the spec's "do not mix" rule — the
        // tenant_id migration is added as version 3 in the SAME
        // scheme rather than the modern identifier framework.
        // Idempotent: gated on `current < 3`; existing rows get the
        // reserved 'default' tenant.
        if current < 3 {
            crate::db::ensure_tenant_id_column(conn, "plan_approvals")?;
            crate::db::ensure_tenant_id_column(conn, "plan_verifications")?;
            crate::db::record_migration_applied(conn, 3)?;
        }
        Ok(())
    }

    /// Persist a single [`VerificationEntry`]. Used by the
    /// verification harness as each step completes.
    /// GROUP 6: tenant-blind insert — writes the reserved
    /// `'default'` tenant. New code should prefer
    /// [`Self::insert_verification_for_tenant`].
    pub fn insert_verification(&self, entry: &VerificationEntry) -> Result<(), ApprovalError> {
        self.insert_verification_for_tenant(entry, "default")
    }

    /// GROUP 6: insert attributed to the caller's VERIFIED tenant.
    pub fn insert_verification_for_tenant(
        &self,
        entry: &VerificationEntry,
        tenant_id: &str,
    ) -> Result<(), ApprovalError> {
        let tenant = if tenant_id.trim().is_empty() {
            "default"
        } else {
            tenant_id
        };
        let conn = self.lock();
        conn.execute(
            "INSERT INTO plan_verifications \
             (plan_id, step_id, criterion, strategy_used, passed, reason, verified_at_ms, tenant_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                entry.plan_id,
                entry.step_id,
                entry.criterion,
                entry.strategy_used,
                if entry.passed { 1 } else { 0 },
                entry.reason,
                entry.verified_at_ms,
                tenant,
            ],
        )?;
        Ok(())
    }

    /// GROUP 6: tenant-scoped count of verification rows for a
    /// plan — proves cross-tenant denial in SQL.
    pub fn count_verifications_for_tenant(
        &self,
        tenant: &str,
        plan_id: &str,
    ) -> Result<u64, ApprovalError> {
        let conn = self.lock();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM plan_verifications WHERE tenant_id = ?1 AND plan_id = ?2",
            params![tenant, plan_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// List every verification entry for `plan_id`, ordered
    /// by `verified_at_ms` ascending so operators read the
    /// log in chronological order.
    pub fn list_verifications(
        &self,
        plan_id: &str,
    ) -> Result<Vec<VerificationEntry>, ApprovalError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT plan_id, step_id, criterion, strategy_used, passed, reason, verified_at_ms \
             FROM plan_verifications WHERE plan_id = ?1 ORDER BY verified_at_ms ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![plan_id], |row| {
            Ok(VerificationEntry {
                plan_id: row.get(0)?,
                step_id: row.get(1)?,
                criterion: row.get(2)?,
                strategy_used: row.get(3)?,
                passed: row.get::<_, i64>(4)? != 0,
                reason: row.get(5)?,
                verified_at_ms: row.get(6)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(ApprovalError::Sqlite)
    }
}

/// One row of the verification log.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerificationEntry {
    pub plan_id: String,
    pub step_id: String,
    pub criterion: String,
    /// One of `"length_check"`, `"keyword_presence"`,
    /// `"keyword_absence"`, `"pattern_match"`, `"ai_judge"`.
    pub strategy_used: String,
    pub passed: bool,
    /// Human-readable explanation of why the criterion
    /// passed or failed. For LengthCheck this includes the
    /// observed count vs. the limit; for KeywordPresence /
    /// KeywordAbsence the matched / missing keyword; for
    /// AiJudge the verifier's returned `reason` field.
    pub reason: String,
    pub verified_at_ms: i64,
}

impl ApprovalStore {
    /// Insert a new pending approval. Errors if a record with
    /// the same `plan_id` already exists.
    pub fn insert_pending(&self, record: &ApprovalRecord) -> Result<(), ApprovalError> {
        self.insert_pending_for_tenant(record, "default")
    }

    /// GROUP 6: insert attributed to the caller's VERIFIED tenant.
    pub fn insert_pending_for_tenant(
        &self,
        record: &ApprovalRecord,
        tenant_id: &str,
    ) -> Result<(), ApprovalError> {
        let tenant = if tenant_id.trim().is_empty() {
            "default"
        } else {
            tenant_id
        };
        let conn = self.lock();
        let spec_json = serde_json::to_string(&record.spec)?;
        let orch_json = match &record.orchestrator_meta {
            serde_json::Value::Null => None,
            v => Some(serde_json::to_string(v)?),
        };
        let critic_json = match &record.critic_meta {
            serde_json::Value::Null => None,
            v => Some(serde_json::to_string(v)?),
        };
        conn.execute(
            "INSERT INTO plan_approvals \
             (id, spec_json, workflow_yaml, status, created_at_ms, decided_at_ms, decision_note, orchestrator_meta, critic_meta, tenant_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                record.plan_id,
                spec_json,
                record.workflow_yaml,
                record.status.as_str(),
                record.created_at_ms,
                record.decided_at_ms,
                record.decision_note,
                orch_json,
                critic_json,
                tenant,
            ],
        )?;
        Ok(())
    }

    /// GROUP 6: tenant-scoped lookup — returns the record ONLY
    /// when it belongs to `tenant`, so a caller scoped to tenant
    /// A cannot read tenant B's plan approval by id.
    pub fn get_for_tenant(
        &self,
        plan_id: &str,
        tenant: &str,
    ) -> Result<Option<ApprovalRecord>, ApprovalError> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, spec_json, workflow_yaml, status, created_at_ms, decided_at_ms, \
                    decision_note, orchestrator_meta, critic_meta \
             FROM plan_approvals WHERE id = ?1 AND tenant_id = ?2",
            params![plan_id, tenant],
            row_to_record,
        )
        .optional()
        .map_err(Into::into)
        .and_then(|opt| opt.transpose())
    }

    /// Look up one record by plan id. `None` when no row
    /// matches.
    pub fn get(&self, plan_id: &str) -> Result<Option<ApprovalRecord>, ApprovalError> {
        let conn = self.lock();
        conn.query_row(
            "SELECT id, spec_json, workflow_yaml, status, created_at_ms, decided_at_ms, \
                    decision_note, orchestrator_meta, critic_meta \
             FROM plan_approvals WHERE id = ?1",
            params![plan_id],
            row_to_record,
        )
        .optional()
        .map_err(Into::into)
        .and_then(|opt| opt.transpose())
    }

    /// List records, optionally filtered by status. Always
    /// returns newest-first (descending `created_at_ms`).
    pub fn list(
        &self,
        status_filter: Option<ApprovalStatus>,
    ) -> Result<Vec<ApprovalRecord>, ApprovalError> {
        let conn = self.lock();
        let mut stmt = if status_filter.is_some() {
            conn.prepare(
                "SELECT id, spec_json, workflow_yaml, status, created_at_ms, decided_at_ms, \
                        decision_note, orchestrator_meta, critic_meta \
                 FROM plan_approvals WHERE status = ?1 ORDER BY created_at_ms DESC",
            )?
        } else {
            conn.prepare(
                "SELECT id, spec_json, workflow_yaml, status, created_at_ms, decided_at_ms, \
                        decision_note, orchestrator_meta, critic_meta \
                 FROM plan_approvals ORDER BY created_at_ms DESC",
            )?
        };
        let rows = if let Some(s) = status_filter {
            stmt.query_map(params![s.as_str()], row_to_record)?
                .collect::<Result<Result<Vec<_>, _>, _>>()
        } else {
            stmt.query_map([], row_to_record)?
                .collect::<Result<Result<Vec<_>, _>, _>>()
        };
        rows.map_err(ApprovalError::Sqlite)?
    }

    /// Transition a pending record to `new_status`. Returns
    /// the resulting record (with the decision_note +
    /// decided_at_ms populated) on success. Returns
    /// [`ApprovalError::NotFound`] when no row matches and
    /// [`ApprovalError::NotPending`] when the existing row is
    /// already `approved` / `rejected` / `expired`.
    pub fn decide(
        &self,
        plan_id: &str,
        new_status: ApprovalStatus,
        note: Option<&str>,
        decided_at_ms: i64,
    ) -> Result<ApprovalRecord, ApprovalError> {
        let mut conn = self.lock();
        // CORR PART 3: BEGIN IMMEDIATE wraps the read-check-
        // write so two concurrent decide calls on the same
        // plan serialise. The UPDATE's `WHERE status = 'pending'`
        // is load-bearing: when zero rows are affected, the
        // transaction lost the race and we return AlreadyDecided
        // instead of falsely reporting success.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing_status: Option<String> = tx
            .query_row(
                "SELECT status FROM plan_approvals WHERE id = ?1",
                params![plan_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(status_str) = existing_status else {
            return Err(ApprovalError::NotFound(plan_id.to_string()));
        };
        let current =
            ApprovalStatus::parse(&status_str).ok_or_else(|| ApprovalError::NotPending {
                plan_id: plan_id.to_string(),
                status: ApprovalStatus::Rejected,
            })?;
        if current != ApprovalStatus::Pending {
            return Err(ApprovalError::NotPending {
                plan_id: plan_id.to_string(),
                status: current,
            });
        }
        let affected = tx.execute(
            "UPDATE plan_approvals \
             SET status = ?1, decided_at_ms = ?2, decision_note = ?3 \
             WHERE id = ?4 AND status = 'pending'",
            params![new_status.as_str(), decided_at_ms, note, plan_id],
        )?;
        if affected == 0 {
            // Lost the race; another decide / expire flipped
            // the row between our read and our UPDATE.
            return Err(ApprovalError::AlreadyDecided(plan_id.to_string()));
        }
        tx.commit()?;
        drop(conn);
        // PART 9: best-effort decision mirror. Re-entry into
        // the generic store is bounded by the
        // `record_decision`-only-flips-pending semantics, so a
        // ↔ b loop stops on the second hop.
        if let Some(mirror) = self.decision_mirror.get() {
            mirror.mirror_decision(plan_id, new_status.as_str(), note);
        }
        // Re-read so we return the canonical row.
        match self.get(plan_id)? {
            Some(r) => Ok(r),
            None => Err(ApprovalError::NotFound(plan_id.to_string())),
        }
    }

    /// Find every `pending` record older than `cutoff_ms` and
    /// transition them to `expired`. Returns the list of
    /// records that were expired so the caller can fan out
    /// notifications.
    pub fn expire_older_than(
        &self,
        cutoff_ms: i64,
        decided_at_ms: i64,
    ) -> Result<Vec<ApprovalRecord>, ApprovalError> {
        let mut conn = self.lock();
        // CORR PART 3: wrap the entire scan-and-update loop in
        // one `BEGIN IMMEDIATE` transaction so a concurrent
        // operator decide can't slip in between our SELECT and
        // the per-row UPDATEs. Pre-fix path took the SQLite
        // statement-level lock per row, which let a race like
        // (operator decides A; expire sees A pending; expire
        // tries to expire A) corrupt the row's decision_note
        // semantics. The `status = 'pending'` clause is still
        // load-bearing inside the transaction — when affected
        // rows == 0 the row was flipped under us by another
        // path (shouldn't happen inside this tx, but defensive).
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let candidates: Vec<ApprovalRecord> = {
            let mut stmt = tx.prepare(
                "SELECT id, spec_json, workflow_yaml, status, created_at_ms, decided_at_ms, \
                        decision_note, orchestrator_meta, critic_meta \
                 FROM plan_approvals \
                 WHERE status = 'pending' AND created_at_ms < ?1",
            )?;
            stmt.query_map(params![cutoff_ms], row_to_record)?
                .collect::<Result<Result<Vec<_>, _>, _>>()
                .map_err(ApprovalError::Sqlite)??
        };
        // PART 2: collapse the per-row UPDATE loop into a
        // single statement. The decision_note depends on each
        // row's created_at_ms, so we compute it inline with
        // SQLite string concatenation. The transaction still
        // serialises this with the prior SELECT, so the
        // candidates list and the UPDATE see the same snapshot
        // of pending rows.
        if !candidates.is_empty() {
            tx.execute(
                "UPDATE plan_approvals \
                 SET status = 'expired', \
                     decided_at_ms = ?1, \
                     decision_note = 'expired after ' || \
                         ((?1 - created_at_ms) / 1000) || 's (created_at_ms=' || \
                         created_at_ms || ', cutoff_ms=' || ?2 || ')' \
                 WHERE status = 'pending' AND created_at_ms < ?2",
                params![decided_at_ms, cutoff_ms],
            )?;
        }
        let mut expired = Vec::with_capacity(candidates.len());
        for c in candidates {
            let mut updated = c;
            updated.status = ApprovalStatus::Expired;
            updated.decided_at_ms = Some(decided_at_ms);
            updated.decision_note = Some(format!(
                "expired after {}s",
                (decided_at_ms - updated.created_at_ms) / 1000
            ));
            expired.push(updated);
        }
        tx.commit()?;
        Ok(expired)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn row_to_record(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<ApprovalRecord, ApprovalError>> {
    let plan_id: String = row.get(0)?;
    let spec_json: String = row.get(1)?;
    let workflow_yaml: String = row.get(2)?;
    let status_str: String = row.get(3)?;
    let created_at_ms: i64 = row.get(4)?;
    let decided_at_ms: Option<i64> = row.get(5)?;
    let decision_note: Option<String> = row.get(6)?;
    let orchestrator_meta_str: Option<String> = row.get(7)?;
    let critic_meta_str: Option<String> = row.get(8)?;
    Ok((|| -> Result<ApprovalRecord, ApprovalError> {
        let spec: PlanSpec = serde_json::from_str(&spec_json)?;
        let status = ApprovalStatus::parse(&status_str).unwrap_or(ApprovalStatus::Pending);
        let orchestrator_meta = match orchestrator_meta_str {
            Some(s) => serde_json::from_str(&s)?,
            None => serde_json::Value::Null,
        };
        let critic_meta = match critic_meta_str {
            Some(s) => serde_json::from_str(&s)?,
            None => serde_json::Value::Null,
        };
        Ok(ApprovalRecord {
            plan_id,
            spec,
            workflow_yaml,
            status,
            created_at_ms,
            decided_at_ms,
            decision_note,
            orchestrator_meta,
            critic_meta,
        })
    })())
}

/// One channel target the approval notifier dispatches to.
/// Mirrors [`crate::metrics::alert_delivery::AlertTarget`] so
/// operators write `[[planning.approval_targets]]` rows with
/// the same fields they use for `[[metrics.alerts.targets]]`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ApprovalTarget {
    pub channel: String,
    pub peer: String,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub chat_id: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default, alias = "slack_channel")]
    pub slack_channel: Option<String>,
}

/// Format a pending-plan notification body. Pulled into its
/// own function so the unit tests can lock the exact format
/// the operator sees without standing up a mesh client.
pub fn format_pending_notification(record: &ApprovalRecord, bridge_hint: Option<&str>) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "📝 Relix planning approval needed — plan_id: {}",
        record.plan_id
    );
    let goal_preview: String = record.spec.goal.chars().take(140).collect();
    let _ = writeln!(out, "Goal: {goal_preview}");
    let agent_count = record
        .spec
        .preferred_agents
        .len()
        .max(record.orchestrator_meta_specialist_count());
    if agent_count > 0 {
        let _ = writeln!(out, "Specialists: {agent_count}");
    }
    if record.spec.is_complex {
        let _ = writeln!(
            out,
            "Complexity: {:.2} (above auto-orchestration threshold)",
            record.spec.complexity_score
        );
    }
    let _ = writeln!(
        out,
        "Decide via: relix planning approve {id}  (or reject {id})",
        id = record.plan_id
    );
    if let Some(bridge) = bridge_hint {
        let bridge = bridge.trim_end_matches('/');
        let _ = writeln!(
            out,
            "Or POST {bridge}/v1/planning/approve {{plan_id, note?}}"
        );
    }
    out
}

impl ApprovalRecord {
    /// Convenience: pull the `specialist_count` field out of
    /// the persisted orchestrator metadata. `0` when the field
    /// is absent or the orchestrator was skipped.
    pub fn orchestrator_meta_specialist_count(&self) -> usize {
        self.orchestrator_meta
            .get("specialist_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| {
                self.orchestrator_meta
                    .get("specialist_assignments")
                    .and_then(serde_json::Value::as_array)
                    .map(|a| a.len() as u64)
                    .unwrap_or(0)
            }) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planning::SpecParser;

    fn fixture_record(plan_id: &str, status: ApprovalStatus, created_at_ms: i64) -> ApprovalRecord {
        let mut spec = SpecParser::new().parse("Research the web.");
        // Force the spec_id so we can pin assertions in tests.
        spec.spec_id = plan_id.to_string();
        let _ = spec.sign();
        ApprovalRecord {
            plan_id: plan_id.to_string(),
            spec,
            workflow_yaml: "name: x\nversion: 1\nagents: {}\nflow: {start: a}\n".into(),
            status,
            created_at_ms,
            decided_at_ms: None,
            decision_note: None,
            orchestrator_meta: serde_json::json!({"activated": true, "specialist_count": 2}),
            critic_meta: serde_json::json!({"enabled": true, "rounds": 1, "approved": true}),
        }
    }

    #[test]
    fn group6_planning_reads_are_isolated_by_verified_tenant() {
        // plan_approvals + plan_verifications: two tenants write
        // records; a tenant-scoped read sees ONLY its own.
        let s = ApprovalStore::open_in_memory().unwrap();
        s.insert_pending_for_tenant(
            &fixture_record("plan-a", ApprovalStatus::Pending, 1),
            "tenant-a",
        )
        .unwrap();
        s.insert_pending_for_tenant(
            &fixture_record("plan-b", ApprovalStatus::Pending, 2),
            "tenant-b",
        )
        .unwrap();
        assert!(s.get_for_tenant("plan-a", "tenant-a").unwrap().is_some());
        assert!(
            s.get_for_tenant("plan-b", "tenant-a").unwrap().is_none(),
            "tenant A must not read tenant B's plan approval"
        );
        // plan_verifications isolation.
        s.insert_verification_for_tenant(
            &VerificationEntry {
                plan_id: "shared-plan".into(),
                step_id: "s".into(),
                criterion: "c".into(),
                strategy_used: "length_check".into(),
                passed: true,
                reason: "r".into(),
                verified_at_ms: 1,
            },
            "tenant-a",
        )
        .unwrap();
        s.insert_verification_for_tenant(
            &VerificationEntry {
                plan_id: "shared-plan".into(),
                step_id: "s".into(),
                criterion: "c".into(),
                strategy_used: "length_check".into(),
                passed: true,
                reason: "r".into(),
                verified_at_ms: 2,
            },
            "tenant-b",
        )
        .unwrap();
        assert_eq!(
            s.count_verifications_for_tenant("tenant-a", "shared-plan")
                .unwrap(),
            1
        );
        assert_eq!(
            s.count_verifications_for_tenant("tenant-b", "shared-plan")
                .unwrap(),
            1
        );
    }

    #[test]
    fn open_in_memory_creates_schema() {
        let store = ApprovalStore::open_in_memory().expect("open");
        // No records yet.
        let all = store.list(None).expect("list");
        assert!(all.is_empty());
    }

    #[test]
    fn insert_pending_then_get_round_trips() {
        let store = ApprovalStore::open_in_memory().unwrap();
        let r = fixture_record("plan-a", ApprovalStatus::Pending, 1_000);
        store.insert_pending(&r).unwrap();
        let got = store.get("plan-a").unwrap().expect("found");
        assert_eq!(got.plan_id, "plan-a");
        assert_eq!(got.status, ApprovalStatus::Pending);
        assert_eq!(got.spec.spec_id, "plan-a");
        // Spec signature survives the round trip.
        got.spec.verify().expect("spec verifies after round trip");
    }

    #[test]
    fn list_filters_by_status() {
        let store = ApprovalStore::open_in_memory().unwrap();
        store
            .insert_pending(&fixture_record("a", ApprovalStatus::Pending, 1))
            .unwrap();
        store
            .insert_pending(&fixture_record("b", ApprovalStatus::Pending, 2))
            .unwrap();
        // Decide b → approved.
        store
            .decide("b", ApprovalStatus::Approved, Some("looks good"), 10)
            .unwrap();
        let pending = store.list(Some(ApprovalStatus::Pending)).unwrap();
        let approved = store.list(Some(ApprovalStatus::Approved)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].plan_id, "a");
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].plan_id, "b");
    }

    #[test]
    fn decide_pending_to_approved_records_note_and_timestamp() {
        let store = ApprovalStore::open_in_memory().unwrap();
        store
            .insert_pending(&fixture_record("p", ApprovalStatus::Pending, 100))
            .unwrap();
        let updated = store
            .decide("p", ApprovalStatus::Approved, Some("ship it"), 200)
            .unwrap();
        assert_eq!(updated.status, ApprovalStatus::Approved);
        assert_eq!(updated.decided_at_ms, Some(200));
        assert_eq!(updated.decision_note.as_deref(), Some("ship it"));
    }

    #[test]
    fn decide_on_already_decided_plan_returns_not_pending() {
        let store = ApprovalStore::open_in_memory().unwrap();
        store
            .insert_pending(&fixture_record("p", ApprovalStatus::Pending, 100))
            .unwrap();
        store
            .decide("p", ApprovalStatus::Approved, Some("first"), 200)
            .unwrap();
        let err = store
            .decide("p", ApprovalStatus::Rejected, Some("second"), 300)
            .unwrap_err();
        assert!(matches!(err, ApprovalError::NotPending { .. }), "{err}");
    }

    #[test]
    fn decide_on_unknown_plan_returns_not_found() {
        let store = ApprovalStore::open_in_memory().unwrap();
        let err = store
            .decide("ghost", ApprovalStatus::Approved, None, 1)
            .unwrap_err();
        assert!(matches!(err, ApprovalError::NotFound(_)), "{err}");
    }

    #[test]
    fn expire_older_than_marks_old_pending_as_expired() {
        let store = ApprovalStore::open_in_memory().unwrap();
        // Created at 100ms; cutoff at 1000ms → eligible.
        store
            .insert_pending(&fixture_record("old", ApprovalStatus::Pending, 100))
            .unwrap();
        // Created at 5000ms; cutoff at 1000ms → still pending.
        store
            .insert_pending(&fixture_record("new", ApprovalStatus::Pending, 5_000))
            .unwrap();
        let expired = store.expire_older_than(1_000, 2_000).unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].plan_id, "old");
        let old_record = store.get("old").unwrap().unwrap();
        assert_eq!(old_record.status, ApprovalStatus::Expired);
        // Note auto-populated with timing details.
        let note = old_record.decision_note.unwrap();
        assert!(note.contains("expired"), "note={note}");
        // Newer record is untouched.
        let new_record = store.get("new").unwrap().unwrap();
        assert_eq!(new_record.status, ApprovalStatus::Pending);
    }

    // ── CORR PART 3: race on decide() ────────────────────

    #[test]
    fn corr_p3_concurrent_decide_only_one_succeeds() {
        // Open a file-backed store so two threads contend on
        // the same SQLite file. The `BEGIN IMMEDIATE` taken
        // by `decide` serialises them.
        use std::sync::Arc;
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("approval.db");
        let store = ApprovalStore::open(&p).unwrap();
        store
            .insert_pending(&fixture_record("race-1", ApprovalStatus::Pending, 100))
            .unwrap();
        let s1 = store.clone();
        let s2 = store.clone();
        let r1 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r2 = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r1c = r1.clone();
        let r2c = r2.clone();
        let t1 = std::thread::spawn(move || {
            let res = s1.decide("race-1", ApprovalStatus::Approved, Some("one"), 200);
            if res.is_ok() {
                r1c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            res
        });
        let t2 = std::thread::spawn(move || {
            let res = s2.decide("race-1", ApprovalStatus::Rejected, Some("two"), 201);
            if res.is_ok() {
                r2c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            res
        });
        let r1r = t1.join().unwrap();
        let r2r = t2.join().unwrap();
        // Exactly one OK; the other returns NotPending or
        // AlreadyDecided (both are acceptable loser shapes).
        let oks = r1.load(std::sync::atomic::Ordering::SeqCst)
            + r2.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(oks, 1, "exactly one decide must succeed: {r1r:?} / {r2r:?}");
    }

    #[test]
    fn expire_does_not_overwrite_already_decided_records() {
        let store = ApprovalStore::open_in_memory().unwrap();
        store
            .insert_pending(&fixture_record("p", ApprovalStatus::Pending, 100))
            .unwrap();
        // Decide before sweep runs.
        store
            .decide("p", ApprovalStatus::Approved, Some("ok"), 150)
            .unwrap();
        let expired = store.expire_older_than(1_000, 2_000).unwrap();
        assert!(
            expired.is_empty(),
            "decided records must not be moved to expired"
        );
        let r = store.get("p").unwrap().unwrap();
        assert_eq!(r.status, ApprovalStatus::Approved);
    }

    #[test]
    fn status_round_trips_through_parse_and_as_str() {
        for s in [
            ApprovalStatus::Pending,
            ApprovalStatus::Approved,
            ApprovalStatus::Rejected,
            ApprovalStatus::Expired,
        ] {
            let parsed = ApprovalStatus::parse(s.as_str()).expect("parse");
            assert_eq!(parsed, s);
        }
        assert!(ApprovalStatus::parse("nonsense").is_none());
    }

    #[test]
    fn is_decision_returns_true_for_approved_and_rejected_only() {
        assert!(ApprovalStatus::Approved.is_decision());
        assert!(ApprovalStatus::Rejected.is_decision());
        assert!(!ApprovalStatus::Pending.is_decision());
        assert!(!ApprovalStatus::Expired.is_decision());
    }

    #[test]
    fn format_pending_notification_includes_plan_id_and_goal() {
        let r = fixture_record("abc-123", ApprovalStatus::Pending, 100);
        let msg = format_pending_notification(&r, Some("http://127.0.0.1:19791/"));
        assert!(msg.contains("abc-123"));
        assert!(msg.contains("Research the web"));
        assert!(msg.contains("Specialists: 2"));
        assert!(msg.contains("relix planning approve abc-123"));
        assert!(msg.contains("http://127.0.0.1:19791/v1/planning/approve"));
    }
}
