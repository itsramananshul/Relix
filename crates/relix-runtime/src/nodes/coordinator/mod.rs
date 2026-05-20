//! Coordinator node — durable Task records over SQLite.
//!
//! Capabilities registered on a controller with `[controller] node_type =
//! "coordinator"`:
//!
//! - `task.create`  — mint a Task record (status = `pending`).
//! - `task.update`  — mutate status / result / flow pointer.
//! - `task.event`   — append a free-form event to a Task's history.
//! - `task.get`     — read one Task and its event chronicle.
//! - `task.list`    — page through Task summaries.
//!
//! ## What "checkpointed re-run" actually means (read this first)
//!
//! The Coordinator is a **durable Task ledger**, not a resumable execution
//! engine. It persists:
//!
//! - Task records (who, what, current status, latest result, pointers to
//!   the per-flow event log of the most recent execution attempt).
//! - A free-form event stream per task (`task.event`) suitable for
//!   checkpoint-style observations the caller wants to remember across
//!   restarts.
//!
//! It does **not** persist:
//!
//! - Mid-flow VM state. The alpha SOL VM is synchronous (`SIMP-001`,
//!   `SIMP-014`); there is no durable yield model yet. If a flow is
//!   interrupted, the caller can re-run it from the start — what changed
//!   is that the Task record survives and the previous attempt's flow
//!   event log is preserved on disk and pointed to by the Task.
//!
//! In other words: durable orchestration *metadata*, ephemeral
//! orchestration *execution*. Real resumable replay lands when the SOL
//! VM gains a durable yield model (Gate 2 spec target).
//!
//! ## Wire format (SIMP-016 alpha — UTF-8 strings)
//!
//! All capabilities use pipe-delimited UTF-8 strings, matching the
//! convention used by `memory.*` and `ai.chat`. Empty fields are valid
//! (skip a field by leaving its slot empty: `||x|||`).
//!
//! | Method | Arg | Returns |
//! |---|---|---|
//! | `task.create` | `title\|flow_template\|params_json\|owner_subject_id` | `task_id` (32-hex) |
//! | `task.update` | `task_id\|status\|result\|flow_id\|flow_log_path\|error_kind\|error_cause` | `ok\n` |
//! | `task.event`  | `task_id\|event_type\|payload` | `event_id` (integer as string) |
//! | `task.get`    | `task_id` | multi-line `key=value` summary + `events:` JSON array |
//! | `task.list`   | `` (empty) or `limit` (default 50) | one `task_id\tstatus\ttitle\n` per line |
//!
//! All times are unix seconds. `status` is opaque — common values:
//! `pending`, `running`, `completed`, `failed`, `abandoned`. The
//! Coordinator does not enforce a state machine; status discipline is
//! the caller's responsibility (the bridge implements the canonical
//! transitions; ad-hoc callers via `relix-cli task` can write anything).
//!
//! ## Trust model
//!
//! The Coordinator is a regular peer. Every call goes through the
//! standard admission pipeline (identity → policy → handler → audit).
//! Two intentional simplifications today:
//!
//! - The Coordinator does not verify that the `owner_subject_id` passed
//!   to `task.create` matches the caller's `subject_id`. Operators who
//!   care can wire a policy rule that pins `task.create` to a group
//!   that admits only the bridge.
//! - The Coordinator persists what callers tell it. A malicious caller
//!   inside the same trust root can write any `payload` they want via
//!   `task.event`. The audit log on the Coordinator records every
//!   call, so post-hoc investigation is possible.
//!
//! ## Not in scope (deliberate)
//!
//! - No automatic re-launch on Coordinator restart. Tasks left in
//!   `running` after a crash stay in `running`; an operator must
//!   `update` them to `abandoned` or `retry` them manually. Automatic
//!   recovery needs both (a) a real resume model and (b) a leadership
//!   election among potential executors — both Gate 2.
//! - No fan-out / scheduling. The Coordinator is a record-keeper.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

/// Per-node coordinator configuration parsed from `[coordinator]`.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct CoordinatorConfig {
    /// SQLite database path. Parent directory created on first start.
    pub db_path: PathBuf,
    /// Maximum tasks `task.list` will return in one call regardless of
    /// the caller's request. Defaults to 200.
    #[serde(default = "default_max_list")]
    pub max_list: usize,
}

fn default_max_list() -> usize {
    200
}

/// SQLite-backed Task ledger. `rusqlite::Connection` is not `Sync`, so
/// the connection lives inside an `Arc<Mutex<_>>`; the bridge serialises
/// access. Volume is low (one row per task, one row per checkpoint) so
/// the mutex isn't a bottleneck in the alpha.
pub struct TaskStore {
    conn: Arc<Mutex<Connection>>,
    max_list: usize,
}

impl TaskStore {
    /// Open or create a task store at the configured path.
    pub fn open(cfg: &CoordinatorConfig) -> Result<Self, CoordinatorError> {
        if let Some(parent) = cfg.db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CoordinatorError::Io(e.to_string()))?;
        }
        let conn = Connection::open(&cfg.db_path).map_err(CoordinatorError::Db)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            max_list: cfg.max_list.max(1),
        })
    }

    /// In-memory backend for unit tests.
    pub fn in_memory() -> Result<Self, CoordinatorError> {
        let conn = Connection::open_in_memory().map_err(CoordinatorError::Db)?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            max_list: 200,
        })
    }

    /// Insert a new Task. Returns the freshly-minted `task_id` (32 hex chars).
    pub fn create(
        &self,
        title: &str,
        flow_template: &str,
        params_json: &str,
        owner_subject_id: &str,
    ) -> Result<String, CoordinatorError> {
        let task_id = new_task_id();
        let now = unix_secs();
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        conn.execute(
            "INSERT INTO tasks (task_id, title, status, owner_subject_id,
                                flow_template, params_json,
                                created_at, updated_at)
             VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6, ?6)",
            params![
                task_id,
                title,
                owner_subject_id,
                flow_template,
                params_json,
                now
            ],
        )
        .map_err(CoordinatorError::Db)?;
        Ok(task_id)
    }

    /// Mutate a Task. Any of `status` / `result` / `flow_id` /
    /// `flow_log_path` / `error_kind` / `error_cause` may be `None`, in
    /// which case the existing value is preserved.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &self,
        task_id: &str,
        status: Option<&str>,
        result: Option<&str>,
        flow_id: Option<&str>,
        flow_log_path: Option<&str>,
        error_kind: Option<i64>,
        error_cause: Option<&str>,
    ) -> Result<(), CoordinatorError> {
        let now = unix_secs();
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        // Build a dynamic UPDATE with only the supplied fields. Avoids
        // overwriting unrelated columns with NULL on partial updates.
        let mut sets: Vec<&str> = vec!["updated_at = ?"];
        let mut args: Vec<rusqlite::types::Value> = vec![now.into()];
        if let Some(v) = status {
            sets.push("status = ?");
            args.push(v.to_string().into());
        }
        if let Some(v) = result {
            sets.push("latest_result = ?");
            args.push(v.to_string().into());
        }
        if let Some(v) = flow_id {
            sets.push("latest_flow_id = ?");
            args.push(v.to_string().into());
        }
        if let Some(v) = flow_log_path {
            sets.push("latest_flow_log_path = ?");
            args.push(v.to_string().into());
        }
        if let Some(v) = error_kind {
            sets.push("error_kind = ?");
            args.push(v.into());
        }
        if let Some(v) = error_cause {
            sets.push("error_cause = ?");
            args.push(v.to_string().into());
        }
        args.push(task_id.to_string().into());
        let sql = format!("UPDATE tasks SET {} WHERE task_id = ?", sets.join(", "));
        let n = conn
            .execute(&sql, rusqlite::params_from_iter(args.iter()))
            .map_err(CoordinatorError::Db)?;
        if n == 0 {
            return Err(CoordinatorError::NotFound(task_id.to_string()));
        }
        Ok(())
    }

    /// Append a free-form event to a Task's history. Returns the
    /// monotonically-increasing event id.
    pub fn append_event(
        &self,
        task_id: &str,
        event_type: &str,
        payload: &str,
    ) -> Result<i64, CoordinatorError> {
        let now = unix_secs();
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        // Make sure the task exists so we don't accumulate orphan rows.
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .map_err(CoordinatorError::Db)?;
        if exists == 0 {
            return Err(CoordinatorError::NotFound(task_id.to_string()));
        }
        conn.execute(
            "INSERT INTO task_events (task_id, ts, event_type, payload)
             VALUES (?1, ?2, ?3, ?4)",
            params![task_id, now, event_type, payload],
        )
        .map_err(CoordinatorError::Db)?;
        Ok(conn.last_insert_rowid())
    }

    /// Read one Task plus its event chronicle. Returns `None` when the
    /// task id is unknown.
    pub fn get(&self, task_id: &str) -> Result<Option<TaskView>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let row = conn
            .query_row(
                "SELECT title, status, owner_subject_id, flow_template,
                        params_json, latest_result, latest_flow_id,
                        latest_flow_log_path, error_kind, error_cause,
                        created_at, updated_at
                 FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| {
                    Ok(TaskView {
                        task_id: task_id.to_string(),
                        title: r.get(0)?,
                        status: r.get(1)?,
                        owner_subject_id: r.get(2)?,
                        flow_template: r.get(3)?,
                        params_json: r.get(4)?,
                        latest_result: r.get(5)?,
                        latest_flow_id: r.get(6)?,
                        latest_flow_log_path: r.get(7)?,
                        error_kind: r.get(8)?,
                        error_cause: r.get(9)?,
                        created_at: r.get(10)?,
                        updated_at: r.get(11)?,
                        events: Vec::new(),
                    })
                },
            )
            .optional()
            .map_err(CoordinatorError::Db)?;
        let Some(mut view) = row else {
            return Ok(None);
        };
        let mut stmt = conn
            .prepare(
                "SELECT event_id, ts, event_type, payload
                 FROM task_events WHERE task_id = ?1 ORDER BY event_id ASC",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![task_id], |r| {
                Ok(TaskEvent {
                    event_id: r.get(0)?,
                    ts: r.get(1)?,
                    event_type: r.get(2)?,
                    payload: r.get(3)?,
                })
            })
            .map_err(CoordinatorError::Db)?;
        for r in rows {
            view.events.push(r.map_err(CoordinatorError::Db)?);
        }
        Ok(Some(view))
    }

    /// Most-recently-updated tasks first, capped at `min(limit, max_list)`.
    pub fn list(&self, limit: usize) -> Result<Vec<TaskSummary>, CoordinatorError> {
        let cap = limit.clamp(1, self.max_list);
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT task_id, title, status, updated_at
                 FROM tasks ORDER BY updated_at DESC LIMIT ?1",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![cap as i64], |r| {
                Ok(TaskSummary {
                    task_id: r.get(0)?,
                    title: r.get(1)?,
                    status: r.get(2)?,
                    updated_at: r.get(3)?,
                })
            })
            .map_err(CoordinatorError::Db)?;
        let mut out = Vec::with_capacity(cap);
        for r in rows {
            out.push(r.map_err(CoordinatorError::Db)?);
        }
        Ok(out)
    }
}

// ──────────────────────────── View types ────────────────────────────────────

/// One Task plus its event history. Returned by `task.get`.
#[derive(Debug, Clone)]
pub struct TaskView {
    pub task_id: String,
    pub title: String,
    pub status: String,
    pub owner_subject_id: String,
    pub flow_template: String,
    pub params_json: String,
    pub latest_result: Option<String>,
    pub latest_flow_id: Option<String>,
    pub latest_flow_log_path: Option<String>,
    pub error_kind: Option<i64>,
    pub error_cause: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub events: Vec<TaskEvent>,
}

/// One event appended via `task.event`.
#[derive(Debug, Clone)]
pub struct TaskEvent {
    pub event_id: i64,
    pub ts: i64,
    pub event_type: String,
    pub payload: String,
}

/// Compact Task representation returned by `task.list`.
#[derive(Debug, Clone)]
pub struct TaskSummary {
    pub task_id: String,
    pub title: String,
    pub status: String,
    pub updated_at: i64,
}

// ──────────────────────────── Capability registration ──────────────────────

/// Register the five task capabilities on the dispatch bridge.
pub fn register(bridge: &mut DispatchBridge, store: Arc<TaskStore>) {
    {
        let s = store.clone();
        bridge.register(
            "task.create",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_create(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.update",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_update(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.event",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_event(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.get",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_get(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.list",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_list(&s, &ctx) }
            })),
        );
    }
}

// ──────────────────────────── Handlers ──────────────────────────────────────

fn handle_create(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.create utf8: {e}")),
    };
    // `title|flow_template|params_json|owner_subject_id`. We splitn(4) so
    // params_json can contain `|`.
    let mut parts = s.splitn(4, '|');
    let title = parts.next().unwrap_or("");
    let flow_template = parts.next().unwrap_or("");
    let params_json = parts.next().unwrap_or("");
    let owner = parts.next().unwrap_or("");
    if title.is_empty() || flow_template.is_empty() {
        return invalid("task.create: `title` and `flow_template` are required".to_string());
    }
    let owner = if owner.is_empty() {
        // Default to the caller's subject_id. Operators who want to
        // pin an owner from elsewhere can still pass one explicitly.
        ctx.caller.subject_id.to_string()
    } else {
        owner.to_string()
    };
    match store.create(title, flow_template, params_json, &owner) {
        Ok(id) => HandlerOutcome::Ok(id.into_bytes()),
        Err(e) => internal(format!("task.create: {e}")),
    }
}

fn handle_update(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.update utf8: {e}")),
    };
    // `task_id|status|result|flow_id|flow_log_path|error_kind|error_cause`
    let parts: Vec<&str> = s.splitn(7, '|').collect();
    let get = |i: usize| -> Option<&str> { parts.get(i).copied().filter(|v| !v.is_empty()) };
    let Some(task_id) = get(0) else {
        return invalid("task.update: task_id required".to_string());
    };
    let status = get(1);
    let result = get(2);
    let flow_id = get(3);
    let flow_log_path = get(4);
    let error_kind = get(5).and_then(|v| v.parse::<i64>().ok());
    let error_cause = get(6);
    match store.update(
        task_id,
        status,
        result,
        flow_id,
        flow_log_path,
        error_kind,
        error_cause,
    ) {
        Ok(()) => HandlerOutcome::Ok(b"ok\n".to_vec()),
        Err(CoordinatorError::NotFound(id)) => invalid(format!("task.update: not found: {id}")),
        Err(e) => internal(format!("task.update: {e}")),
    }
}

fn handle_event(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.event utf8: {e}")),
    };
    // `task_id|event_type|payload`. Payload may contain `|`.
    let mut parts = s.splitn(3, '|');
    let task_id = parts.next().unwrap_or("");
    let event_type = parts.next().unwrap_or("");
    let payload = parts.next().unwrap_or("");
    if task_id.is_empty() || event_type.is_empty() {
        return invalid("task.event: task_id and event_type required".to_string());
    }
    match store.append_event(task_id, event_type, payload) {
        Ok(id) => HandlerOutcome::Ok(id.to_string().into_bytes()),
        Err(CoordinatorError::NotFound(id)) => invalid(format!("task.event: not found: {id}")),
        Err(e) => internal(format!("task.event: {e}")),
    }
}

fn handle_get(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.get utf8: {e}")),
    };
    if task_id.is_empty() {
        return invalid("task.get: task_id required".to_string());
    }
    match store.get(task_id) {
        Ok(Some(view)) => HandlerOutcome::Ok(render_task_view(&view).into_bytes()),
        Ok(None) => invalid(format!("task.get: not found: {task_id}")),
        Err(e) => internal(format!("task.get: {e}")),
    }
}

fn handle_list(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.list utf8: {e}")),
    };
    let limit: usize = if s.is_empty() {
        50
    } else {
        s.parse().unwrap_or(50)
    };
    match store.list(limit) {
        Ok(rows) => {
            let mut buf = String::new();
            for r in rows {
                buf.push_str(&r.task_id);
                buf.push('\t');
                buf.push_str(&r.status);
                buf.push('\t');
                // Title may contain tabs in theory; sanitise minimally.
                let title = r.title.replace(['\t', '\n'], " ");
                buf.push_str(&title);
                buf.push('\n');
            }
            HandlerOutcome::Ok(buf.into_bytes())
        }
        Err(e) => internal(format!("task.list: {e}")),
    }
}

/// Render a TaskView as a multi-line `key=value` block followed by an
/// `events:` JSON array. Stable + grep-friendly for `relix-cli task get`.
fn render_task_view(v: &TaskView) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "task_id={}", v.task_id);
    let _ = writeln!(s, "title={}", v.title);
    let _ = writeln!(s, "status={}", v.status);
    let _ = writeln!(s, "owner_subject_id={}", v.owner_subject_id);
    let _ = writeln!(s, "flow_template={}", v.flow_template);
    let _ = writeln!(s, "params_json={}", v.params_json);
    if let Some(x) = v.latest_result.as_ref() {
        let _ = writeln!(s, "latest_result={}", x);
    }
    if let Some(x) = v.latest_flow_id.as_ref() {
        let _ = writeln!(s, "latest_flow_id={}", x);
    }
    if let Some(x) = v.latest_flow_log_path.as_ref() {
        let _ = writeln!(s, "latest_flow_log_path={}", x);
    }
    if let Some(x) = v.error_kind {
        let _ = writeln!(s, "error_kind={}", x);
    }
    if let Some(x) = v.error_cause.as_ref() {
        let _ = writeln!(s, "error_cause={}", x);
    }
    let _ = writeln!(s, "created_at={}", v.created_at);
    let _ = writeln!(s, "updated_at={}", v.updated_at);
    let _ = writeln!(s, "event_count={}", v.events.len());
    // Events as a simple JSON array. We hand-build the JSON to avoid
    // pulling serde_json into this hot path; payloads are escaped
    // minimally. Operators wanting structured payloads should keep them
    // already-encoded inside the payload string.
    s.push_str("events=[");
    for (i, ev) in v.events.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            r#"{{"id":{},"ts":{},"type":"{}","payload":"{}"}}"#,
            ev.event_id,
            ev.ts,
            json_escape(&ev.event_type),
            json_escape(&ev.payload),
        );
    }
    s.push_str("]\n");
    s
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn invalid(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

fn internal(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause,
        retry_hint: 1,
        retry_after: None,
    })
}

// ──────────────────────────── Schema + helpers ──────────────────────────────

fn init_schema(conn: &Connection) -> Result<(), CoordinatorError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS tasks (
            task_id              TEXT PRIMARY KEY,
            title                TEXT NOT NULL,
            status               TEXT NOT NULL,
            owner_subject_id     TEXT NOT NULL,
            flow_template        TEXT NOT NULL,
            params_json          TEXT NOT NULL,
            latest_result        TEXT,
            latest_flow_id       TEXT,
            latest_flow_log_path TEXT,
            error_kind           INTEGER,
            error_cause          TEXT,
            created_at           INTEGER NOT NULL,
            updated_at           INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS tasks_updated ON tasks(updated_at DESC);

        CREATE TABLE IF NOT EXISTS task_events (
            event_id   INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id    TEXT    NOT NULL,
            ts         INTEGER NOT NULL,
            event_type TEXT    NOT NULL,
            payload    TEXT    NOT NULL,
            FOREIGN KEY (task_id) REFERENCES tasks(task_id)
        );
        CREATE INDEX IF NOT EXISTS task_events_task ON task_events(task_id, event_id);
        "#,
    )
    .map_err(CoordinatorError::Db)
}

fn new_task_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// rusqlite::OptionalExtension brings `.optional()` into scope; importing
// here keeps the trait local rather than re-exporting it everywhere.
use rusqlite::OptionalExtension as _;

// ──────────────────────────── Errors ────────────────────────────────────────

/// Coordinator-node errors.
#[derive(Debug, thiserror::Error)]
pub enum CoordinatorError {
    /// File-system failure preparing the DB path.
    #[error("io: {0}")]
    Io(String),
    /// SQLite failure.
    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),
    /// Mutex poisoned (programmer error; logged for visibility).
    #[error("lock poisoned")]
    Lock,
    /// Task id has no matching row.
    #[error("task not found: {0}")]
    NotFound(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> TaskStore {
        TaskStore::in_memory().expect("open")
    }

    #[test]
    fn create_and_get_roundtrip() {
        let s = store();
        let tid = s
            .create("demo task", "chat_template.sol", "{}", "owner-xyz")
            .unwrap();
        assert_eq!(tid.len(), 32);
        let v = s.get(&tid).unwrap().expect("present");
        assert_eq!(v.title, "demo task");
        assert_eq!(v.status, "pending");
        assert_eq!(v.flow_template, "chat_template.sol");
        assert_eq!(v.owner_subject_id, "owner-xyz");
        assert_eq!(v.events.len(), 0);
    }

    #[test]
    fn update_preserves_unset_fields() {
        let s = store();
        let tid = s.create("t", "f", "{}", "o").unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None)
            .unwrap();
        s.update(
            &tid,
            None,
            Some("the result body"),
            Some("flowabc"),
            Some("/tmp/x.log"),
            None,
            None,
        )
        .unwrap();
        let v = s.get(&tid).unwrap().expect("present");
        assert_eq!(v.status, "running");
        assert_eq!(v.latest_result.as_deref(), Some("the result body"));
        assert_eq!(v.latest_flow_id.as_deref(), Some("flowabc"));
        assert_eq!(v.latest_flow_log_path.as_deref(), Some("/tmp/x.log"));
    }

    #[test]
    fn events_append_and_read_back_in_order() {
        let s = store();
        let tid = s.create("t", "f", "{}", "o").unwrap();
        let e1 = s.append_event(&tid, "step", "memory.write_turn").unwrap();
        let e2 = s.append_event(&tid, "step", "ai.chat").unwrap();
        let e3 = s
            .append_event(&tid, "checkpoint", "history=18 chars")
            .unwrap();
        assert!(e2 > e1 && e3 > e2);
        let v = s.get(&tid).unwrap().expect("present");
        assert_eq!(v.events.len(), 3);
        assert_eq!(v.events[0].event_type, "step");
        assert_eq!(v.events[0].payload, "memory.write_turn");
        assert_eq!(v.events[2].event_type, "checkpoint");
    }

    #[test]
    fn list_returns_most_recently_updated_first() {
        let s = store();
        let _ = s.create("first", "f", "{}", "o").unwrap();
        let second = s.create("second", "f", "{}", "o").unwrap();
        let _ = s.create("third", "f", "{}", "o").unwrap();
        // Touch the second so it bubbles to the top.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        s.update(&second, Some("completed"), None, None, None, None, None)
            .unwrap();
        let rows = s.list(10).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].title, "second");
    }

    #[test]
    fn update_unknown_task_is_invalid() {
        let s = store();
        match s.update("deadbeef", Some("running"), None, None, None, None, None) {
            Err(CoordinatorError::NotFound(id)) => assert_eq!(id, "deadbeef"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn event_on_unknown_task_is_invalid() {
        let s = store();
        match s.append_event("deadbeef", "step", "x") {
            Err(CoordinatorError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn rendered_view_contains_expected_keys() {
        let s = store();
        let tid = s
            .create("render demo", "chat", "{\"a\":1}", "owner-1")
            .unwrap();
        s.append_event(&tid, "checkpoint", "step=1").unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        let rendered = render_task_view(&v);
        assert!(rendered.contains(&format!("task_id={tid}")));
        assert!(rendered.contains("status=pending"));
        assert!(rendered.contains("flow_template=chat"));
        assert!(rendered.contains("event_count=1"));
        assert!(rendered.contains("\"type\":\"checkpoint\""));
        assert!(rendered.contains("\"payload\":\"step=1\""));
    }
}
