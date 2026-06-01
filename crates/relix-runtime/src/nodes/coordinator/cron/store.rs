//! SQLite-backed store for cron jobs.
//!
//! Opens its own `rusqlite::Connection` against the same
//! database file the [`crate::nodes::coordinator::TaskStore`]
//! uses. SQLite handles cross-connection locking; we deliberately
//! do NOT thread the same `Arc<Mutex<Connection>>` through both
//! stores because their access patterns (the task store is hot,
//! the cron store is cool) don't share contention concerns.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use super::schedule::{Schedule, ScheduleError};

/// One scheduled job row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronJob {
    pub job_id: String,
    pub name: String,
    /// Verbatim schedule expression — the scheduler re-parses
    /// on every tick so an `cron.update` that changes the
    /// expression takes effect immediately.
    pub schedule: String,
    pub flow_template: String,
    pub prompt: String,
    pub subject_id: String,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_run_at: Option<i64>,
    pub next_run_at: i64,
    pub run_count: i64,
    pub last_task_id: Option<String>,
    pub last_status: Option<String>,
}

/// Lightweight view used by `cron.list` so we don't materialise
/// the prompt / flow_template strings just to render a list row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronJobSummary {
    pub job_id: String,
    pub name: String,
    pub schedule: String,
    pub next_run_at: i64,
    pub last_run_at: Option<i64>,
    pub enabled: bool,
    pub run_count: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum CronStoreError {
    #[error("cron store: {0}")]
    Io(String),
    #[error("cron store: db: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("cron store: schedule: {0}")]
    Schedule(#[from] ScheduleError),
    #[error("cron store: not found: {0}")]
    NotFound(String),
    #[error("cron store: bad input: {0}")]
    BadInput(String),
    #[error("cron store: poisoned mutex")]
    Lock,
}

pub struct CronStore {
    conn: Arc<Mutex<Connection>>,
}

impl CronStore {
    /// Open or create a cron store at `path`. Idempotent schema
    /// — re-running against an existing DB is safe.
    pub fn open(path: &Path) -> Result<Self, CronStoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CronStoreError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::log_integrity_warning(&conn, "cron_store");
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// In-memory backend for unit tests.
    pub fn in_memory() -> Result<Self, CronStoreError> {
        let conn = Connection::open_in_memory()?;
        crate::db::apply_pragmas(&conn)?;
        crate::db::ensure_migration_table(&conn)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Validate + parse a schedule expression and insert a new
    /// job. Returns the freshly-minted 16-hex `job_id`.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        name: &str,
        schedule_expr: &str,
        flow_template: &str,
        prompt: &str,
        subject_id: &str,
        // GROUP 6: caller's VERIFIED tenant (from InvocationCtx).
        tenant_id: &str,
    ) -> Result<String, CronStoreError> {
        let tenant = if tenant_id.trim().is_empty() {
            "default"
        } else {
            tenant_id
        };
        if name.trim().is_empty() {
            return Err(CronStoreError::BadInput("name required".into()));
        }
        if flow_template.trim().is_empty() {
            return Err(CronStoreError::BadInput("flow_template required".into()));
        }
        if subject_id.trim().is_empty() {
            return Err(CronStoreError::BadInput("subject_id required".into()));
        }
        let schedule = Schedule::parse(schedule_expr)?;
        let now = unix_now();
        let next = schedule.next_after(now);
        let job_id = new_job_id();
        let conn = self.conn.lock().map_err(|_| CronStoreError::Lock)?;
        conn.execute(
            "INSERT INTO cron_jobs (
                 job_id, name, schedule, flow_template, prompt, subject_id,
                 enabled, created_at, updated_at, next_run_at, run_count, tenant_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7, ?8, 0, ?9)",
            params![
                job_id,
                name,
                schedule_expr,
                flow_template,
                prompt,
                subject_id,
                now,
                next,
                tenant,
            ],
        )?;
        Ok(job_id)
    }

    /// GROUP 6: tenant-scoped lookup — returns the job ONLY when
    /// it belongs to `tenant`, so a caller scoped to tenant A
    /// can never read tenant B's job even with B's `job_id`.
    pub fn get_for_tenant(
        &self,
        job_id: &str,
        tenant: &str,
    ) -> Result<Option<CronJob>, CronStoreError> {
        let conn = self.conn.lock().map_err(|_| CronStoreError::Lock)?;
        let row = conn
            .query_row(
                "SELECT job_id, name, schedule, flow_template, prompt, subject_id,
                        enabled, created_at, updated_at, last_run_at, next_run_at,
                        run_count, last_task_id, last_status
                 FROM cron_jobs WHERE job_id = ?1 AND tenant_id = ?2",
                params![job_id, tenant],
                row_to_job,
            )
            .optional()?;
        Ok(row)
    }

    pub fn get(&self, job_id: &str) -> Result<Option<CronJob>, CronStoreError> {
        let conn = self.conn.lock().map_err(|_| CronStoreError::Lock)?;
        let row = conn
            .query_row(
                "SELECT job_id, name, schedule, flow_template, prompt, subject_id,
                        enabled, created_at, updated_at, last_run_at, next_run_at,
                        run_count, last_task_id, last_status
                 FROM cron_jobs WHERE job_id = ?1",
                params![job_id],
                row_to_job,
            )
            .optional()?;
        Ok(row)
    }

    /// GROUP 6: list jobs for the caller's VERIFIED `tenant`,
    /// optionally narrowed to one `subject_id`. EVERY row is
    /// filtered by `tenant_id` so a caller never sees another
    /// tenant's jobs. Newest-first by `created_at`.
    pub fn list(
        &self,
        tenant: &str,
        subject_id: Option<&str>,
    ) -> Result<Vec<CronJobSummary>, CronStoreError> {
        let conn = self.conn.lock().map_err(|_| CronStoreError::Lock)?;
        let sql = if subject_id.is_some() {
            "SELECT job_id, name, schedule, next_run_at, last_run_at, enabled, run_count
             FROM cron_jobs WHERE tenant_id = ?1 AND subject_id = ?2 ORDER BY created_at DESC"
        } else {
            "SELECT job_id, name, schedule, next_run_at, last_run_at, enabled, run_count
             FROM cron_jobs WHERE tenant_id = ?1 ORDER BY created_at DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let map = |r: &rusqlite::Row| {
            Ok(CronJobSummary {
                job_id: r.get(0)?,
                name: r.get(1)?,
                schedule: r.get(2)?,
                next_run_at: r.get(3)?,
                last_run_at: r.get(4)?,
                enabled: r.get::<_, i64>(5)? != 0,
                run_count: r.get(6)?,
            })
        };
        let rows: Vec<CronJobSummary> = if let Some(s) = subject_id {
            stmt.query_map(params![tenant, s], map)?
                .collect::<rusqlite::Result<_>>()?
        } else {
            stmt.query_map(params![tenant], map)?
                .collect::<rusqlite::Result<_>>()?
        };
        Ok(rows)
    }

    /// Update one field of a job. Recognised fields:
    ///
    /// - `enabled` — accepts `0` or `1`.
    /// - `schedule` — re-parses; recomputes `next_run_at`.
    /// - `prompt` — replaces the prompt text verbatim.
    ///
    /// Any other field name returns `BadInput`.
    pub fn update_field(
        &self,
        job_id: &str,
        field: &str,
        value: &str,
    ) -> Result<(), CronStoreError> {
        let conn = self.conn.lock().map_err(|_| CronStoreError::Lock)?;
        let now = unix_now();
        match field {
            "enabled" => {
                let v: i64 = value
                    .parse()
                    .map_err(|_| CronStoreError::BadInput(format!("enabled '{value}' not 0/1")))?;
                if v != 0 && v != 1 {
                    return Err(CronStoreError::BadInput(format!(
                        "enabled '{value}' must be 0 or 1"
                    )));
                }
                let changed = conn.execute(
                    "UPDATE cron_jobs SET enabled = ?1, updated_at = ?2 WHERE job_id = ?3",
                    params![v, now, job_id],
                )?;
                if changed == 0 {
                    return Err(CronStoreError::NotFound(job_id.into()));
                }
            }
            "schedule" => {
                let schedule = Schedule::parse(value)?;
                let next = schedule.next_after(now);
                let changed = conn.execute(
                    "UPDATE cron_jobs SET schedule = ?1, next_run_at = ?2, updated_at = ?3
                     WHERE job_id = ?4",
                    params![value, next, now, job_id],
                )?;
                if changed == 0 {
                    return Err(CronStoreError::NotFound(job_id.into()));
                }
            }
            "prompt" => {
                let changed = conn.execute(
                    "UPDATE cron_jobs SET prompt = ?1, updated_at = ?2 WHERE job_id = ?3",
                    params![value, now, job_id],
                )?;
                if changed == 0 {
                    return Err(CronStoreError::NotFound(job_id.into()));
                }
            }
            other => {
                return Err(CronStoreError::BadInput(format!(
                    "unknown field '{other}' (allowed: enabled, schedule, prompt)"
                )));
            }
        }
        Ok(())
    }

    /// Delete a job. Returns `NotFound` if the id doesn't exist.
    pub fn delete(&self, job_id: &str) -> Result<(), CronStoreError> {
        let conn = self.conn.lock().map_err(|_| CronStoreError::Lock)?;
        let changed = conn.execute("DELETE FROM cron_jobs WHERE job_id = ?1", params![job_id])?;
        if changed == 0 {
            return Err(CronStoreError::NotFound(job_id.into()));
        }
        Ok(())
    }

    /// Enabled jobs whose `next_run_at <= now`.
    pub fn due_jobs(&self, now: i64) -> Result<Vec<CronJob>, CronStoreError> {
        let conn = self.conn.lock().map_err(|_| CronStoreError::Lock)?;
        let mut stmt = conn.prepare(
            "SELECT job_id, name, schedule, flow_template, prompt, subject_id,
                    enabled, created_at, updated_at, last_run_at, next_run_at,
                    run_count, last_task_id, last_status
             FROM cron_jobs
             WHERE enabled = 1 AND next_run_at <= ?1
             ORDER BY next_run_at ASC",
        )?;
        let rows: Vec<CronJob> = stmt
            .query_map(params![now], row_to_job)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Stamp a job after the scheduler fires it. Idempotent:
    /// callers may pass `disable = true` for a one-shot to flip
    /// `enabled = 0` in the same transaction.
    pub fn record_fire(
        &self,
        job_id: &str,
        fired_at: i64,
        next_run_at: i64,
        task_id: &str,
        disable_after: bool,
    ) -> Result<(), CronStoreError> {
        let conn = self.conn.lock().map_err(|_| CronStoreError::Lock)?;
        let enabled = if disable_after { 0i64 } else { 1i64 };
        let changed = conn.execute(
            "UPDATE cron_jobs SET
                 last_run_at = ?1,
                 next_run_at = ?2,
                 run_count   = run_count + 1,
                 last_task_id = ?3,
                 last_status = 'running',
                 enabled     = CASE WHEN ?4 = 0 THEN 0 ELSE enabled END,
                 updated_at  = ?1
             WHERE job_id = ?5",
            params![fired_at, next_run_at, task_id, enabled, job_id],
        )?;
        if changed == 0 {
            return Err(CronStoreError::NotFound(job_id.into()));
        }
        Ok(())
    }

    /// Test-only escape hatch for tests that need to poke
    /// the database directly (e.g. forcing `next_run_at` into
    /// the past to make a job due immediately).
    #[cfg(test)]
    pub(crate) fn conn_for_tests(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        })
    }

    /// Stamp the last_status column after a fire completes.
    /// Best-effort — failure to update the status column never
    /// fails the scheduler tick.
    pub fn record_status(&self, job_id: &str, status: &str) -> Result<(), CronStoreError> {
        let conn = self.conn.lock().map_err(|_| CronStoreError::Lock)?;
        let now = unix_now();
        conn.execute(
            "UPDATE cron_jobs SET last_status = ?1, updated_at = ?2 WHERE job_id = ?3",
            params![status, now, job_id],
        )?;
        Ok(())
    }
}

fn init_schema(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS cron_jobs (
             job_id        TEXT PRIMARY KEY,
             name          TEXT NOT NULL,
             schedule      TEXT NOT NULL,
             flow_template TEXT NOT NULL,
             prompt        TEXT NOT NULL,
             subject_id    TEXT NOT NULL,
             enabled       INTEGER NOT NULL DEFAULT 1,
             created_at    INTEGER NOT NULL,
             updated_at    INTEGER NOT NULL,
             last_run_at   INTEGER,
             next_run_at   INTEGER NOT NULL,
             run_count     INTEGER NOT NULL DEFAULT 0,
             last_task_id  TEXT,
             last_status   TEXT
         );
         CREATE INDEX IF NOT EXISTS cron_jobs_due
             ON cron_jobs(enabled, next_run_at);
         CREATE INDEX IF NOT EXISTS cron_jobs_subject
             ON cron_jobs(subject_id, created_at);",
    )?;
    // GROUP 6: tenant isolation column (idempotent).
    crate::db::ensure_tenant_id_column(conn, "cron_jobs")?;
    Ok(())
}

fn row_to_job(r: &rusqlite::Row) -> rusqlite::Result<CronJob> {
    Ok(CronJob {
        job_id: r.get(0)?,
        name: r.get(1)?,
        schedule: r.get(2)?,
        flow_template: r.get(3)?,
        prompt: r.get(4)?,
        subject_id: r.get(5)?,
        enabled: r.get::<_, i64>(6)? != 0,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        last_run_at: r.get(9)?,
        next_run_at: r.get(10)?,
        run_count: r.get(11)?,
        last_task_id: r.get(12)?,
        last_status: r.get(13)?,
    })
}

/// 16-hex `job_id` minted from a fresh 64-bit random + the
/// current unix-seconds. We don't pull in `uuid` for this; the
/// collision risk inside one operator's database is vanishing.
fn new_job_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group6_cron_reads_are_isolated_by_verified_tenant() {
        // Two tenants create jobs for the SAME subject_id. The
        // tenant-scoped get/list must return ONLY the caller's
        // tenant's jobs — never the other tenant's.
        let s = CronStore::in_memory().unwrap();
        let a = s
            .create("a", "1d", "f.sol", "p", "shared-subj", "tenant-a")
            .unwrap();
        let b = s
            .create("b", "1d", "f.sol", "p", "shared-subj", "tenant-b")
            .unwrap();
        // get is tenant-scoped: A cannot read B's job by id.
        assert!(s.get_for_tenant(&a, "tenant-a").unwrap().is_some());
        assert!(
            s.get_for_tenant(&b, "tenant-a").unwrap().is_none(),
            "tenant A must not read tenant B's cron job"
        );
        // list is tenant-scoped: each tenant sees only its own.
        assert_eq!(s.list("tenant-a", Some("shared-subj")).unwrap().len(), 1);
        assert_eq!(s.list("tenant-b", Some("shared-subj")).unwrap().len(), 1);
        assert_eq!(s.list("tenant-a", None).unwrap().len(), 1);
    }

    #[test]
    fn create_then_get_round_trips_every_field() {
        let s = CronStore::in_memory().unwrap();
        let id = s
            .create(
                "daily-summary",
                "1d",
                "flows/chat_template.sol",
                "summarise",
                "subj-1",
                "default",
            )
            .unwrap();
        let j = s.get(&id).unwrap().unwrap();
        assert_eq!(j.job_id, id);
        assert_eq!(j.name, "daily-summary");
        assert_eq!(j.schedule, "1d");
        assert_eq!(j.flow_template, "flows/chat_template.sol");
        assert_eq!(j.prompt, "summarise");
        assert_eq!(j.subject_id, "subj-1");
        assert!(j.enabled);
        assert_eq!(j.run_count, 0);
        assert!(j.last_run_at.is_none());
        assert!(j.next_run_at > j.created_at);
    }

    #[test]
    fn create_rejects_bad_schedule() {
        let s = CronStore::in_memory().unwrap();
        let r = s.create("x", "garbage", "f.sol", "p", "subj", "default");
        assert!(matches!(r, Err(CronStoreError::Schedule(_))));
    }

    #[test]
    fn create_rejects_empty_required_fields() {
        let s = CronStore::in_memory().unwrap();
        assert!(matches!(
            s.create("", "1d", "f.sol", "p", "subj", "default"),
            Err(CronStoreError::BadInput(_))
        ));
        assert!(matches!(
            s.create("n", "1d", "", "p", "subj", "default"),
            Err(CronStoreError::BadInput(_))
        ));
        assert!(matches!(
            s.create("n", "1d", "f.sol", "p", "", "default"),
            Err(CronStoreError::BadInput(_))
        ));
    }

    #[test]
    fn list_filters_by_subject_id() {
        let s = CronStore::in_memory().unwrap();
        s.create("a", "1d", "f.sol", "p", "subj-1", "default")
            .unwrap();
        s.create("b", "1d", "f.sol", "p", "subj-2", "default")
            .unwrap();
        s.create("c", "1d", "f.sol", "p", "subj-1", "default")
            .unwrap();
        let one = s.list("default", Some("subj-1")).unwrap();
        assert_eq!(one.len(), 2);
        for r in &one {
            assert!(r.name == "a" || r.name == "c");
        }
        let two = s.list("default", Some("subj-2")).unwrap();
        assert_eq!(two.len(), 1);
        assert_eq!(two[0].name, "b");
    }

    #[test]
    fn list_with_none_returns_all_subjects() {
        let s = CronStore::in_memory().unwrap();
        s.create("a", "1d", "f.sol", "p", "subj-1", "default")
            .unwrap();
        s.create("b", "1d", "f.sol", "p", "subj-2", "default")
            .unwrap();
        assert_eq!(s.list("default", None).unwrap().len(), 2);
    }

    #[test]
    fn update_enabled_disables_then_reenables() {
        let s = CronStore::in_memory().unwrap();
        let id = s
            .create("a", "1d", "f.sol", "p", "subj", "default")
            .unwrap();
        s.update_field(&id, "enabled", "0").unwrap();
        assert!(!s.get(&id).unwrap().unwrap().enabled);
        s.update_field(&id, "enabled", "1").unwrap();
        assert!(s.get(&id).unwrap().unwrap().enabled);
    }

    #[test]
    fn update_schedule_recomputes_next_run_at() {
        let s = CronStore::in_memory().unwrap();
        let id = s
            .create("a", "1d", "f.sol", "p", "subj", "default")
            .unwrap();
        let before = s.get(&id).unwrap().unwrap().next_run_at;
        s.update_field(&id, "schedule", "30m").unwrap();
        let after = s.get(&id).unwrap().unwrap();
        assert_eq!(after.schedule, "30m");
        // Old next was ~1d out; new should be ~30m out, strictly closer.
        assert!(after.next_run_at < before);
    }

    #[test]
    fn update_unknown_field_rejected() {
        let s = CronStore::in_memory().unwrap();
        let id = s
            .create("a", "1d", "f.sol", "p", "subj", "default")
            .unwrap();
        assert!(matches!(
            s.update_field(&id, "name", "different"),
            Err(CronStoreError::BadInput(_))
        ));
    }

    #[test]
    fn update_unknown_job_returns_not_found() {
        let s = CronStore::in_memory().unwrap();
        assert!(matches!(
            s.update_field("nope", "enabled", "0"),
            Err(CronStoreError::NotFound(_))
        ));
    }

    #[test]
    fn delete_removes_row_then_returns_not_found_next_time() {
        let s = CronStore::in_memory().unwrap();
        let id = s
            .create("a", "1d", "f.sol", "p", "subj", "default")
            .unwrap();
        s.delete(&id).unwrap();
        assert!(s.get(&id).unwrap().is_none());
        assert!(matches!(s.delete(&id), Err(CronStoreError::NotFound(_))));
    }

    #[test]
    fn due_jobs_returns_only_jobs_whose_next_run_is_past() {
        let s = CronStore::in_memory().unwrap();
        // Create a job, then force its next_run_at into the past.
        let id_due = s
            .create("due", "1d", "f.sol", "p", "subj", "default")
            .unwrap();
        let id_future = s
            .create("future", "1d", "f.sol", "p", "subj", "default")
            .unwrap();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE cron_jobs SET next_run_at = 100 WHERE job_id = ?1",
                params![id_due],
            )
            .unwrap();
        }
        let due = s.due_jobs(1000).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].job_id, id_due);
        // future one is not yet due
        assert!(!due.iter().any(|j| j.job_id == id_future));
    }

    #[test]
    fn due_jobs_excludes_disabled_rows() {
        let s = CronStore::in_memory().unwrap();
        let id = s
            .create("a", "1d", "f.sol", "p", "subj", "default")
            .unwrap();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE cron_jobs SET next_run_at = 0, enabled = 0 WHERE job_id = ?1",
                params![id],
            )
            .unwrap();
        }
        let due = s.due_jobs(1000).unwrap();
        assert!(due.is_empty());
    }

    #[test]
    fn record_fire_advances_run_count_and_optionally_disables() {
        let s = CronStore::in_memory().unwrap();
        let id = s
            .create(
                "once",
                "2026-06-01T00:00:00Z",
                "f.sol",
                "p",
                "subj",
                "default",
            )
            .unwrap();
        s.record_fire(&id, 5_000, 6_000, "task-1", true).unwrap();
        let j = s.get(&id).unwrap().unwrap();
        assert_eq!(j.run_count, 1);
        assert_eq!(j.last_run_at, Some(5_000));
        assert_eq!(j.next_run_at, 6_000);
        assert_eq!(j.last_task_id.as_deref(), Some("task-1"));
        assert!(!j.enabled, "one-shot should be disabled after fire");
    }

    #[test]
    fn record_fire_recurring_keeps_job_enabled() {
        let s = CronStore::in_memory().unwrap();
        let id = s
            .create("daily", "1d", "f.sol", "p", "subj", "default")
            .unwrap();
        s.record_fire(&id, 5_000, 9_000, "task-1", false).unwrap();
        let j = s.get(&id).unwrap().unwrap();
        assert!(j.enabled);
        assert_eq!(j.run_count, 1);
    }

    #[test]
    fn record_status_updates_last_status_text() {
        let s = CronStore::in_memory().unwrap();
        let id = s
            .create("a", "1d", "f.sol", "p", "subj", "default")
            .unwrap();
        s.record_status(&id, "completed").unwrap();
        let j = s.get(&id).unwrap().unwrap();
        assert_eq!(j.last_status.as_deref(), Some("completed"));
    }

    #[test]
    fn new_job_id_is_16_hex_chars() {
        let id = new_job_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn two_consecutive_new_job_ids_differ() {
        // Vanishingly unlikely to collide; the test guards
        // against a degenerate RNG.
        assert_ne!(new_job_id(), new_job_id());
    }
}
