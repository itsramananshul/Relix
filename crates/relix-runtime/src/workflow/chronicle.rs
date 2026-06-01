//! Workflow execution chronicle. Persists each
//! [`crate::workflow::executor::WorkflowResult`] to a small
//! sqlite table keyed by execution id, so `workflow.status`
//! can look up an execution after it ran (and after the
//! coordinator restarts).
//!
//! Stored alongside the task chronicle in the controller's
//! data directory but in its own file (`workflows.sqlite`)
//! so workflow lifecycle doesn't entangle with the task
//! schema's migration cadence.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::executor::{ExecutionStep, ExecutionTrace, WorkflowResult};

#[derive(Debug, Clone, thiserror::Error)]
pub enum ChronicleError {
    #[error("workflow chronicle io: {0}")]
    Io(String),

    #[error("workflow chronicle sqlite: {0}")]
    Db(String),

    #[error("workflow chronicle encode: {0}")]
    Encode(String),
}

#[derive(Clone)]
pub struct WorkflowChronicle {
    conn: Arc<Mutex<Connection>>,
}

/// Serializable form of a single trace step. Matches
/// [`ExecutionStep`] exactly; carries an `error` instead of
/// `Result<(), String>` so JSON consumers don't need to
/// understand Rust's `Result`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub agent: String,
    pub peer: String,
    pub capability: String,
    pub input: String,
    pub output: String,
    pub latency_ms: u64,
    /// `None` on success; `Some(cause)` on failure.
    pub error: Option<String>,
}

impl From<&ExecutionStep> for StepRecord {
    fn from(s: &ExecutionStep) -> Self {
        Self {
            agent: s.agent.clone(),
            peer: s.peer.clone(),
            capability: s.capability.clone(),
            input: s.input.clone(),
            output: s.output.clone(),
            latency_ms: s.latency_ms,
            error: s.outcome.as_ref().err().cloned(),
        }
    }
}

/// Full record returned by [`WorkflowChronicle::get`]. The
/// JSON shape doubles as the response body for
/// `workflow.status` and `workflow.run`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub execution_id: String,
    pub workflow_name: String,
    pub input: String,
    pub status: String,
    pub result: String,
    pub started_at: i64,
    pub ended_at: i64,
    pub total_latency_ms: u64,
    pub steps: Vec<StepRecord>,
}

impl WorkflowChronicle {
    pub fn open(path: &Path) -> Result<Self, ChronicleError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ChronicleError::Io(e.to_string()))?;
        }
        let conn = Connection::open(path).map_err(|e| ChronicleError::Db(e.to_string()))?;
        crate::db::apply_pragmas(&conn).map_err(|e| ChronicleError::Db(e.to_string()))?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open an in-memory chronicle for unit tests.
    pub fn in_memory() -> Result<Self, ChronicleError> {
        let conn = Connection::open_in_memory().map_err(|e| ChronicleError::Db(e.to_string()))?;
        crate::db::apply_pragmas(&conn).map_err(|e| ChronicleError::Db(e.to_string()))?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn init_schema(conn: &Connection) -> Result<(), ChronicleError> {
        // CORR PART 2: drive schema through the identifier-
        // based migration framework so the workflow chronicle
        // schema is tracked in `_relix_migrations`. The legacy
        // claim makes a node that upgrades from the pre-fix
        // path stamp v1 without re-running the CREATE.
        crate::db::claim_legacy_migration(conn, "workflow_chronicle.v1", |c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='table' AND name='workflow_executions'",
                [],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
        .map_err(|e| ChronicleError::Db(e.to_string()))?;
        let body_sql = "CREATE TABLE IF NOT EXISTS workflow_executions (\n                execution_id      TEXT PRIMARY KEY,\n                workflow_name     TEXT NOT NULL,\n                input             TEXT NOT NULL,\n                status            TEXT NOT NULL,\n                result            TEXT NOT NULL,\n                started_at        INTEGER NOT NULL,\n                ended_at          INTEGER NOT NULL,\n                total_latency_ms  INTEGER NOT NULL,\n                steps_json        TEXT NOT NULL\n            );\n            CREATE INDEX IF NOT EXISTS workflow_executions_name\n                ON workflow_executions(workflow_name, started_at DESC);";
        if !crate::db::is_migration_applied(conn, "workflow_chronicle.v1")
            .map_err(|e| ChronicleError::Db(e.to_string()))?
        {
            conn.execute_batch(body_sql)
                .map_err(|e| ChronicleError::Db(e.to_string()))?;
            crate::db::record_migration_applied_by_id(
                conn,
                "workflow_chronicle.v1",
                &crate::db::checksum_sql(body_sql),
            )
            .map_err(|e| ChronicleError::Db(e.to_string()))?;
        }
        // GROUP 6: tenant isolation. Add `tenant_id` so each
        // execution is attributed to the caller's VERIFIED tenant
        // and reads can be tenant-scoped. Idempotent via the
        // column probe; existing rows default to the reserved
        // 'default' tenant (safe: pre-multi-tenant rows, and
        // single-tenant deployments read as "default").
        if !chronicle_column_exists(conn, "workflow_executions", "tenant_id")? {
            conn.execute(
                "ALTER TABLE workflow_executions ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default'",
                [],
            )
            .map_err(|e| ChronicleError::Db(e.to_string()))?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS workflow_executions_tenant \
                 ON workflow_executions(tenant_id, started_at DESC);",
        )
        .map_err(|e| ChronicleError::Db(e.to_string()))?;
        Ok(())
    }

    /// Persist one finished execution. `tenant_id` is the
    /// caller's VERIFIED tenant (from `InvocationCtx`), stored so
    /// reads can be tenant-scoped.
    pub fn record(
        &self,
        result: &WorkflowResult,
        input: &str,
        started_at_unix: i64,
        ended_at_unix: i64,
        tenant_id: &str,
    ) -> Result<(), ChronicleError> {
        let steps: Vec<StepRecord> = result.trace.steps.iter().map(StepRecord::from).collect();
        let steps_json =
            serde_json::to_string(&steps).map_err(|e| ChronicleError::Encode(e.to_string()))?;
        let status = result.status.as_str();
        let tenant = if tenant_id.trim().is_empty() {
            "default"
        } else {
            tenant_id
        };
        let conn = self
            .conn
            .lock()
            .map_err(|_| ChronicleError::Db("workflow chronicle lock poisoned".to_string()))?;
        conn.execute(
            r#"
            INSERT OR REPLACE INTO workflow_executions
              (execution_id, workflow_name, input, status, result,
               started_at, ended_at, total_latency_ms, steps_json, tenant_id)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            "#,
            params![
                result.trace.execution_id.0,
                result.trace.workflow_name,
                input,
                status,
                result.result,
                started_at_unix,
                ended_at_unix,
                result.trace.total_latency_ms as i64,
                steps_json,
                tenant,
            ],
        )
        .map(|_| ())
        .map_err(|e| ChronicleError::Db(e.to_string()))
    }

    /// GROUP 6: tenant-scoped lookup. Returns the execution ONLY
    /// when it belongs to `tenant` — a caller scoped to tenant A
    /// asking for tenant B's `execution_id` gets `None`, never
    /// B's row. The `tenant_id = ?` predicate is in SQL, so the
    /// isolation holds even though the id is a shared key.
    pub fn get_for_tenant(
        &self,
        execution_id: &str,
        tenant: &str,
    ) -> Result<Option<ExecutionRecord>, ChronicleError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| ChronicleError::Db("workflow chronicle lock poisoned".to_string()))?;
        conn.query_row(
            r#"
            SELECT execution_id, workflow_name, input, status, result,
                   started_at, ended_at, total_latency_ms, steps_json
            FROM workflow_executions
            WHERE execution_id = ?1 AND tenant_id = ?2
            "#,
            params![execution_id, tenant],
            |row| {
                let steps_json: String = row.get(8)?;
                let steps: Vec<StepRecord> = serde_json::from_str(&steps_json).unwrap_or_default();
                Ok(ExecutionRecord {
                    execution_id: row.get(0)?,
                    workflow_name: row.get(1)?,
                    input: row.get(2)?,
                    status: row.get(3)?,
                    result: row.get(4)?,
                    started_at: row.get(5)?,
                    ended_at: row.get(6)?,
                    total_latency_ms: row.get::<_, i64>(7)? as u64,
                    steps,
                })
            },
        )
        .optional()
        .map_err(|e| ChronicleError::Db(e.to_string()))
    }

    /// Look up an execution record by id. Returns `Ok(None)`
    /// when the id is unknown.
    pub fn get(&self, execution_id: &str) -> Result<Option<ExecutionRecord>, ChronicleError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| ChronicleError::Db("workflow chronicle lock poisoned".to_string()))?;
        conn.query_row(
            r#"
            SELECT execution_id, workflow_name, input, status, result,
                   started_at, ended_at, total_latency_ms, steps_json
            FROM workflow_executions
            WHERE execution_id = ?1
            "#,
            params![execution_id],
            |row| {
                let steps_json: String = row.get(8)?;
                Ok(ExecutionRecord {
                    execution_id: row.get(0)?,
                    workflow_name: row.get(1)?,
                    input: row.get(2)?,
                    status: row.get(3)?,
                    result: row.get(4)?,
                    started_at: row.get(5)?,
                    ended_at: row.get(6)?,
                    total_latency_ms: row.get::<_, i64>(7)? as u64,
                    steps: serde_json::from_str(&steps_json).unwrap_or_default(),
                })
            },
        )
        .optional()
        .map_err(|e| ChronicleError::Db(e.to_string()))
    }
}

/// Build the canonical [`ExecutionRecord`] from an
/// in-memory [`WorkflowResult`] without going through
/// sqlite — used by `workflow.run` to return the execution
/// to the caller in the same shape `workflow.status` would.
pub fn record_from(
    result: &WorkflowResult,
    input: &str,
    started_at: i64,
    ended_at: i64,
) -> ExecutionRecord {
    let steps: Vec<StepRecord> = result.trace.steps.iter().map(StepRecord::from).collect();
    let status = result.status.as_str().to_string();
    ExecutionRecord {
        execution_id: result.trace.execution_id.0.clone(),
        workflow_name: result.trace.workflow_name.clone(),
        input: input.to_string(),
        status,
        result: result.result.clone(),
        started_at,
        ended_at,
        total_latency_ms: result.trace.total_latency_ms,
        steps,
    }
}

#[allow(dead_code)]
fn _trace_unused_hint(_t: &ExecutionTrace) {}

/// GROUP 6: probe whether `table` has `column` — the idempotent
/// guard for the additive `tenant_id` migration.
fn chronicle_column_exists(
    conn: &Connection,
    table: &str,
    column: &str,
) -> Result<bool, ChronicleError> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| ChronicleError::Db(e.to_string()))?;
    let mut rows = stmt
        .query([])
        .map_err(|e| ChronicleError::Db(e.to_string()))?;
    while let Some(row) = rows.next().map_err(|e| ChronicleError::Db(e.to_string()))? {
        let name: String = row.get(1).map_err(|e| ChronicleError::Db(e.to_string()))?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Default chronicle path under a data dir.
pub fn default_chronicle_path(data_dir: &Path) -> PathBuf {
    data_dir.join("workflows.sqlite")
}

#[cfg(test)]
mod group6_tenant_tests {
    use super::*;
    use crate::workflow::executor::{ExecutionId, ExecutionStatus, ExecutionTrace, WorkflowResult};

    fn result_with_id(id: &str, name: &str) -> WorkflowResult {
        WorkflowResult {
            trace: ExecutionTrace {
                execution_id: ExecutionId(id.to_string()),
                workflow_name: name.to_string(),
                total_latency_ms: 1,
                steps: vec![],
            },
            status: ExecutionStatus::Success,
            result: "ok".to_string(),
        }
    }

    #[test]
    fn group6_workflow_reads_are_isolated_by_verified_tenant() {
        let ch = WorkflowChronicle::in_memory().unwrap();
        ch.record(&result_with_id("exec-A", "wf"), "in", 1, 2, "tenant-a")
            .unwrap();
        ch.record(&result_with_id("exec-B", "wf"), "in", 1, 2, "tenant-b")
            .unwrap();
        // Tenant A sees its own execution...
        assert!(ch.get_for_tenant("exec-A", "tenant-a").unwrap().is_some());
        // ...but CANNOT read tenant B's execution even with B's id.
        assert!(
            ch.get_for_tenant("exec-B", "tenant-a").unwrap().is_none(),
            "tenant A must not read tenant B's workflow execution"
        );
        assert!(ch.get_for_tenant("exec-B", "tenant-b").unwrap().is_some());
    }
}
