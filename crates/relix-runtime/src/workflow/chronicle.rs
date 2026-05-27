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

use super::executor::{ExecutionStatus, ExecutionStep, ExecutionTrace, WorkflowResult};

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
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS workflow_executions (
                execution_id      TEXT PRIMARY KEY,
                workflow_name     TEXT NOT NULL,
                input             TEXT NOT NULL,
                status            TEXT NOT NULL,
                result            TEXT NOT NULL,
                started_at        INTEGER NOT NULL,
                ended_at          INTEGER NOT NULL,
                total_latency_ms  INTEGER NOT NULL,
                steps_json        TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS workflow_executions_name
                ON workflow_executions(workflow_name, started_at DESC);
            "#,
        )
        .map_err(|e| ChronicleError::Db(e.to_string()))
    }

    /// Persist one finished execution.
    pub fn record(
        &self,
        result: &WorkflowResult,
        input: &str,
        started_at_unix: i64,
        ended_at_unix: i64,
    ) -> Result<(), ChronicleError> {
        let steps: Vec<StepRecord> = result.trace.steps.iter().map(StepRecord::from).collect();
        let steps_json =
            serde_json::to_string(&steps).map_err(|e| ChronicleError::Encode(e.to_string()))?;
        let status = match result.status {
            ExecutionStatus::Success => "success",
            ExecutionStatus::Failed => "failed",
        };
        let conn = self
            .conn
            .lock()
            .map_err(|_| ChronicleError::Db("workflow chronicle lock poisoned".to_string()))?;
        conn.execute(
            r#"
            INSERT OR REPLACE INTO workflow_executions
              (execution_id, workflow_name, input, status, result,
               started_at, ended_at, total_latency_ms, steps_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
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
            ],
        )
        .map(|_| ())
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
    let status = match result.status {
        ExecutionStatus::Success => "success",
        ExecutionStatus::Failed => "failed",
    }
    .to_string();
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

/// Default chronicle path under a data dir.
pub fn default_chronicle_path(data_dir: &Path) -> PathBuf {
    data_dir.join("workflows.sqlite")
}
