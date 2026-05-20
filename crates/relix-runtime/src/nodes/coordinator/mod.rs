//! Coordinator node — durable Task records over SQLite.
//!
//! ## Lifecycle vocabulary (C1)
//!
//! Status is an opaque string at the database level — the Coordinator
//! does not enforce a state machine. The bridge + CLI use this
//! convention:
//!
//! | Status            | Meaning |
//! |---|---|
//! | `pending`         | Task created, no execution attempted yet. |
//! | `running`         | An executor took ownership and is running the flow now. |
//! | `retrying`        | A previous attempt failed; another attempt is scheduled (operator-initiated in the alpha). |
//! | `interrupted`     | Executor died or the task's `max_runtime_secs` was exceeded. Coordinator's startup recovery scan flips stale `running` tasks here. |
//! | `awaiting_input`  | Flow paused on an external dependency (human approval, async webhook, etc.). The alpha records the state; the runtime does not yet implement the resume primitive. |
//! | `completed`       | Final attempt succeeded. `latest_result` holds the reply. |
//! | `failed`          | Final attempt failed and the task will not retry. `last_failure_class` and `last_failure_reason` are filled. |
//! | `cancelled`       | Operator explicitly cancelled an active task. |
//!
//! Callers can write any string — these are just the values tooling
//! understands. State-machine enforcement lands at Gate 2 with the
//! resumable VM.
//!
//! ## Failure classes (C1)
//!
//! When a flow fails, the bridge classifies the cause into one of:
//!
//! | Class           | Meaning |
//! |---|---|
//! | `transient`     | Network blip, peer momentarily unreachable, etc. Safe to retry. |
//! | `permanent`     | Logic / contract error inside the flow or a responder. Retry won't help. |
//! | `policy_denied` | Admission pipeline rejected the call. Retry won't help unless policy or identity changes. |
//! | `invalid_args`  | Caller-side input was malformed. Retry won't help. |
//! | `timeout`       | Deadline exceeded. May or may not help to retry. |
//! | `unavailable`   | Capability or peer reported it can't serve right now. |
//!
//! Stored in `last_failure_class` on the Task. Operators use it (via
//! `relix-cli task get` / `task list --status failed`) to decide
//! whether retry is worth it. The runtime does not auto-retry today
//! — bounded auto-retry is a follow-up (see `docs/retry-model.md`).
//!
//! Capabilities registered on a controller with `[controller] node_type =
//! "coordinator"`:
//!
//! - `task.create`  — mint a Task record (status = `pending`).
//! - `task.update`  — mutate status / result / flow pointer.
//! - `task.event`   — append a free-form event to a Task's history.
//! - `task.get`     — read one Task and its event chronicle.
//! - `task.list`    — page through Task summaries.
//! - `task.recover` — operator-triggered version of the startup scan;
//!   promotes overdue `running` tasks to `interrupted`.
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
//! | `task.create`   | `title\|flow_template\|params_json\|owner_subject_id\|retry_policy\|max_retries\|max_runtime_secs` | `task_id` (32-hex) |
//! | `task.update`   | `task_id\|status\|result\|flow_id\|flow_log_path\|error_kind\|error_cause\|failure_class\|trace_id` | `ok\n` |
//! | `task.event`    | `task_id\|event_type\|payload` | `event_id` (integer as string) |
//! | `task.get`      | `task_id` | multi-line `key=value` summary + `events:` JSON array |
//! | `task.list`     | `` (empty) or `limit` (default 50) | one `task_id\tstatus\ttitle\n` per line |
//! | `task.recover`  | (empty) | one `task_id\n` per recovered task, then `recovered=N\n` |
//! | `task.attempts` | `task_id` | one `attempt_num\tstatus\tstarted_at\tfinished_at\|-\tfailure_class\|-\tflow_id\|-\n` per attempt |
//!
//! Optional trailers (older callers that omit them keep working
//! unchanged): `retry_policy|max_retries|max_runtime_secs` on
//! `task.create`; `failure_class|trace_id` on `task.update`.
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
//! - No automatic **re-launch** on Coordinator restart. The C1b
//!   startup scan promotes tasks past their `max_runtime_secs` from
//!   `running` to `interrupted` (so dashboards stop showing a
//!   never-finishing flow), but does NOT execute them again — the
//!   operator decides whether to re-run from the start, and only the
//!   bridge (or `relix-cli flow-run`) can actually execute a flow.
//!   Real re-launch needs both (a) a durable VM resume model and (b)
//!   a leadership election among potential executors — both Gate 2.
//! - No fan-out / scheduling. The Coordinator is a record-keeper.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

/// Lightweight classification of why a flow failed. Written to
/// `tasks.last_failure_class` by the bridge so operators (and any
/// future auto-retry policy) can decide whether the failure is worth
/// retrying. The mapping from `relix_core::types::error_kinds::*` to
/// this enum is the bridge's job; the Coordinator just persists
/// whatever string comes in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureClass {
    /// Network blip, peer momentarily unreachable. Safe to retry.
    Transient,
    /// Logic / contract error. Retry won't help.
    Permanent,
    /// Admission pipeline refused. Retry won't help without identity / policy change.
    PolicyDenied,
    /// Caller-side input was malformed.
    InvalidArgs,
    /// Deadline exceeded.
    Timeout,
    /// Capability or peer signalled it can't serve right now.
    Unavailable,
}

impl FailureClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Permanent => "permanent",
            Self::PolicyDenied => "policy_denied",
            Self::InvalidArgs => "invalid_args",
            Self::Timeout => "timeout",
            Self::Unavailable => "unavailable",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "transient" => Some(Self::Transient),
            "permanent" => Some(Self::Permanent),
            "policy_denied" => Some(Self::PolicyDenied),
            "invalid_args" => Some(Self::InvalidArgs),
            "timeout" => Some(Self::Timeout),
            "unavailable" => Some(Self::Unavailable),
            _ => None,
        }
    }

    /// Convenience mapping from `relix_core::types::error_kinds::*` to
    /// a class. Used by the bridge so it can call
    /// `TaskRecorder::fail(_, FailureClass::from_kind(kind), cause)`
    /// without re-implementing the switch at every call site.
    pub fn from_kind(kind: u32) -> Self {
        use relix_core::types::error_kinds as ek;
        match kind {
            ek::TRANSPORT | ek::PEER_UNREACHABLE | ek::RESPONDER_OVERLOADED => Self::Transient,
            ek::TIMEOUT | ek::APPROVAL_TIMEOUT => Self::Timeout,
            ek::POLICY_DENIED
            | ek::CREDENTIAL_EXPIRED
            | ek::IDENTITY_INVALID
            | ek::APPROVAL_DENIED => Self::PolicyDenied,
            ek::INVALID_ARGS | ek::REPLAY_REJECTED | ek::VERSION_MISMATCH => Self::InvalidArgs,
            ek::UNKNOWN_METHOD
            | ek::CAPABILITY_DEPRECATED
            | ek::CAPABILITY_REMOVED
            | ek::MANIFEST_STALE => Self::Unavailable,
            ek::RESPONDER_INTERNAL | ek::CANCELLED => Self::Permanent,
            _ => Self::Permanent,
        }
    }
}

impl std::fmt::Display for FailureClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Retry policy for a Task. Stored as text in the `tasks` table; the
/// Coordinator does NOT auto-retry today. Operators decide; this is
/// hints + metadata for them, not an executor primitive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryPolicy {
    /// Never retry. Default for backwards compatibility with pre-C1 Tasks.
    None,
    /// One retry permitted on transient-class failures.
    Once,
    /// Up to `max_retries` retries permitted, transient class only.
    Bounded,
}

impl RetryPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Once => "once",
            Self::Bounded => "bounded",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" | "" => Some(Self::None),
            "once" => Some(Self::Once),
            "bounded" => Some(Self::Bounded),
            _ => None,
        }
    }
}

/// Per-node coordinator configuration parsed from `[coordinator]`.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct CoordinatorConfig {
    /// SQLite database path. Parent directory created on first start.
    pub db_path: PathBuf,
    /// Maximum tasks `task.list` will return in one call regardless of
    /// the caller's request. Defaults to 200.
    #[serde(default = "default_max_list")]
    pub max_list: usize,
    /// Run the interruption-recovery scan once at coordinator startup.
    /// Defaults to `true`. Operators can disable it for forensic
    /// investigation (keep stale `running` rows in place) by setting
    /// `recovery_scan = false`. The on-demand `task.recover` capability
    /// is unaffected.
    #[serde(default = "default_recovery_scan")]
    pub recovery_scan: bool,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            max_list: default_max_list(),
            recovery_scan: default_recovery_scan(),
        }
    }
}

fn default_max_list() -> usize {
    200
}

fn default_recovery_scan() -> bool {
    true
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

    /// Insert a new Task. Returns the freshly-minted `task_id`
    /// (32 hex chars). Optional retry / timeout metadata defaults to
    /// "no retry, no timeout" for backwards compatibility with pre-C1
    /// callers that don't supply them.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        title: &str,
        flow_template: &str,
        params_json: &str,
        owner_subject_id: &str,
        retry_policy: RetryPolicy,
        max_retries: i64,
        max_runtime_secs: Option<i64>,
    ) -> Result<String, CoordinatorError> {
        let task_id = new_task_id();
        let now = unix_secs();
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        conn.execute(
            "INSERT INTO tasks (task_id, title, status, owner_subject_id,
                                flow_template, params_json,
                                created_at, updated_at,
                                retry_count, retry_policy, max_retries,
                                max_runtime_secs)
             VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6, ?6,
                     0, ?7, ?8, ?9)",
            params![
                task_id,
                title,
                owner_subject_id,
                flow_template,
                params_json,
                now,
                retry_policy.as_str(),
                max_retries,
                max_runtime_secs,
            ],
        )
        .map_err(CoordinatorError::Db)?;
        Ok(task_id)
    }

    /// Mutate a Task. Any of `status` / `result` / `flow_id` /
    /// `flow_log_path` / `error_kind` / `error_cause` /
    /// `failure_class` may be `None`, in which case the existing
    /// value is preserved. `trace_id` is only consumed on the
    /// `running` transition that opens a new attempt; it's a no-op
    /// otherwise.
    ///
    /// Side effects (all transactional with the row update):
    ///
    /// - `status -> running` with no open attempt opens a new
    ///   attempt row, stamps its `started_at`, and emits a
    ///   `task.attempt_started` event.
    /// - `status -> running` while an attempt is already open is a
    ///   no-op at the attempt level (idempotent on the bridge side).
    /// - `status -> completed | failed | cancelled` closes the open
    ///   attempt with the supplied outcome columns and emits
    ///   `task.attempt_finished`.
    /// - First-ever `status -> running` also stamps the task-level
    ///   `started_at` (one-shot via COALESCE; preserves the
    ///   first-attempt timestamp as the immutable "task first
    ///   started" record).
    /// - Setting `error_cause` mirrors it to `last_failure_reason`
    ///   so the cause survives a later `retrying` transition.
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
        failure_class: Option<&str>,
    ) -> Result<(), CoordinatorError> {
        self.update_with_trace(
            task_id,
            status,
            result,
            flow_id,
            flow_log_path,
            error_kind,
            error_cause,
            failure_class,
            None,
        )
    }

    /// Same as [`update`], but propagates a `trace_id` into the new
    /// attempt row when the call opens one. Separate entry point so
    /// callers that don't have a trace_id keep working unchanged.
    #[allow(clippy::too_many_arguments)]
    pub fn update_with_trace(
        &self,
        task_id: &str,
        status: Option<&str>,
        result: Option<&str>,
        flow_id: Option<&str>,
        flow_log_path: Option<&str>,
        error_kind: Option<i64>,
        error_cause: Option<&str>,
        failure_class: Option<&str>,
        trace_id: Option<&str>,
    ) -> Result<(), CoordinatorError> {
        let now = unix_secs();
        let mut conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let tx = conn.transaction().map_err(CoordinatorError::Db)?;
        let mut sets: Vec<&str> = vec!["updated_at = ?"];
        let mut args: Vec<rusqlite::types::Value> = vec![now.into()];
        if let Some(v) = status {
            sets.push("status = ?");
            args.push(v.to_string().into());
            if v == "running" {
                // Stamp the task-level started_at on the FIRST ever
                // 'running' transition. Per-attempt timing lives on
                // task_attempts.started_at; tasks.started_at is the
                // immutable "first run" record (and what
                // recover_interrupted falls back to for tasks pre-C2a).
                sets.push("started_at = COALESCE(started_at, ?)");
                args.push(now.into());
            }
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
            sets.push("last_failure_reason = ?");
            args.push(v.to_string().into());
        }
        if let Some(v) = failure_class {
            sets.push("last_failure_class = ?");
            args.push(v.to_string().into());
        }
        args.push(task_id.to_string().into());
        let sql = format!("UPDATE tasks SET {} WHERE task_id = ?", sets.join(", "));
        let n = tx
            .execute(&sql, rusqlite::params_from_iter(args.iter()))
            .map_err(CoordinatorError::Db)?;
        if n == 0 {
            return Err(CoordinatorError::NotFound(task_id.to_string()));
        }

        // C2a: drive the per-attempt timeline as a side effect of
        // status transitions. Same transaction as the task row update
        // so observers can never see the cached `tasks.current_attempt_id`
        // diverge from the attempts table.
        if let Some(v) = status {
            match v {
                "running" => {
                    open_attempt_if_needed(&tx, task_id, trace_id, now)?;
                }
                "completed" | "failed" | "cancelled" => {
                    close_open_attempt_if_any(
                        &tx,
                        task_id,
                        v,
                        flow_id,
                        flow_log_path,
                        error_kind,
                        error_cause,
                        failure_class,
                        now,
                    )?;
                }
                _ => {}
            }
        }
        tx.commit().map_err(CoordinatorError::Db)?;
        Ok(())
    }

    /// Bump `retry_count` by one. Used by the bridge when it starts a
    /// new attempt (an explicit `retry.started` event lands first to
    /// keep the chronicle self-describing). Returns the new count.
    pub fn bump_retry_count(&self, task_id: &str) -> Result<i64, CoordinatorError> {
        let now = unix_secs();
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let n = conn
            .execute(
                "UPDATE tasks SET retry_count = retry_count + 1, updated_at = ?1
                 WHERE task_id = ?2",
                params![now, task_id],
            )
            .map_err(CoordinatorError::Db)?;
        if n == 0 {
            return Err(CoordinatorError::NotFound(task_id.to_string()));
        }
        let count: i64 = conn
            .query_row(
                "SELECT retry_count FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .map_err(CoordinatorError::Db)?;
        Ok(count)
    }

    /// Find tasks the Coordinator believes are still `running` but
    /// whose deadline has passed, flip them to `interrupted`, close
    /// the open attempt as interrupted, and append `task.interrupted`
    /// + `task.attempt_finished` events. Returns the recovered ids.
    ///
    /// Deadline source preference: the current attempt's `started_at`
    /// (the C2a attempt timeline). For tasks created before C2a (no
    /// `current_attempt_id`) the scan falls back to the task-level
    /// `started_at`. Both code paths require `max_runtime_secs` set;
    /// rows without it are left alone.
    ///
    /// Race-guarded: the UPDATE re-asserts `status = 'running'` so an
    /// in-flight `task.update` from a long-lived executor cannot be
    /// silently overwritten.
    ///
    /// Idempotent: re-running finds nothing because the rows are no
    /// longer `running`.
    pub fn recover_interrupted(&self, now_secs: i64) -> Result<Vec<String>, CoordinatorError> {
        let mut conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT t.task_id,
                        COALESCE(a.started_at, t.started_at) AS scan_started,
                        t.max_runtime_secs,
                        t.current_attempt_id
                 FROM tasks t
                 LEFT JOIN task_attempts a ON a.attempt_id = t.current_attempt_id
                 WHERE t.status = 'running'
                   AND t.max_runtime_secs IS NOT NULL
                   AND COALESCE(a.started_at, t.started_at) IS NOT NULL
                   AND (COALESCE(a.started_at, t.started_at) + t.max_runtime_secs) < ?1",
            )
            .map_err(CoordinatorError::Db)?;
        let candidates: Vec<(String, i64, i64, Option<i64>)> = stmt
            .query_map(params![now_secs], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            })
            .map_err(CoordinatorError::Db)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(CoordinatorError::Db)?;
        drop(stmt);

        let tx = conn.transaction().map_err(CoordinatorError::Db)?;
        let mut recovered = Vec::with_capacity(candidates.len());
        for (tid, started, max, current_attempt) in candidates {
            let n = tx
                .execute(
                    "UPDATE tasks
                     SET status = 'interrupted',
                         updated_at = ?1,
                         last_failure_reason = ?2,
                         last_failure_class = 'timeout'
                     WHERE task_id = ?3 AND status = 'running'",
                    params![
                        now_secs,
                        format!(
                            "deadline_exceeded: started_at={started} max_runtime_secs={max} now={now_secs}"
                        ),
                        tid,
                    ],
                )
                .map_err(CoordinatorError::Db)?;
            if n == 0 {
                continue;
            }
            let payload = format!(
                "started_at={started} max_runtime_secs={max} now={now_secs} reason=deadline_exceeded"
            );
            tx.execute(
                "INSERT INTO task_events (task_id, ts, event_type, payload)
                 VALUES (?1, ?2, 'task.interrupted', ?3)",
                params![tid, now_secs, payload],
            )
            .map_err(CoordinatorError::Db)?;
            // Close the per-attempt row, if any, so the attempt
            // timeline stays consistent with the task's status field.
            if let Some(attempt_id) = current_attempt {
                tx.execute(
                    "UPDATE task_attempts
                     SET finished_at = ?1,
                         status = 'interrupted',
                         failure_class = 'timeout',
                         error_cause = ?2
                     WHERE attempt_id = ?3 AND finished_at IS NULL",
                    params![
                        now_secs,
                        format!(
                            "deadline_exceeded: started_at={started} max_runtime_secs={max} now={now_secs}"
                        ),
                        attempt_id,
                    ],
                )
                .map_err(CoordinatorError::Db)?;
                tx.execute(
                    "INSERT INTO task_events (task_id, ts, event_type, payload)
                     VALUES (?1, ?2, 'task.attempt_finished', ?3)",
                    params![
                        tid,
                        now_secs,
                        format!("attempt_id={attempt_id} status=interrupted failure_class=timeout"),
                    ],
                )
                .map_err(CoordinatorError::Db)?;
            }
            recovered.push(tid);
        }
        tx.commit().map_err(CoordinatorError::Db)?;
        Ok(recovered)
    }

    /// List all attempts of a task in chronological order. Returns
    /// an empty Vec when the task has no attempts yet (e.g. it was
    /// created but never transitioned to `running`).
    pub fn list_attempts(&self, task_id: &str) -> Result<Vec<AttemptView>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT attempt_id, attempt_num, started_at, finished_at, status,
                        flow_id, flow_log_path, trace_id,
                        error_kind, error_cause, failure_class
                 FROM task_attempts
                 WHERE task_id = ?1
                 ORDER BY attempt_num ASC",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![task_id], |r| {
                Ok(AttemptView {
                    attempt_id: r.get(0)?,
                    attempt_num: r.get(1)?,
                    started_at: r.get(2)?,
                    finished_at: r.get(3)?,
                    status: r.get(4)?,
                    flow_id: r.get(5)?,
                    flow_log_path: r.get(6)?,
                    trace_id: r.get(7)?,
                    error_kind: r.get(8)?,
                    error_cause: r.get(9)?,
                    failure_class: r.get(10)?,
                })
            })
            .map_err(CoordinatorError::Db)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(CoordinatorError::Db)?);
        }
        Ok(out)
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
                        created_at, updated_at,
                        retry_count, retry_policy, max_retries,
                        max_runtime_secs, last_failure_reason,
                        last_failure_class, started_at,
                        attempt_count, current_attempt_id
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
                        retry_count: r.get(12)?,
                        retry_policy: r.get(13)?,
                        max_retries: r.get(14)?,
                        max_runtime_secs: r.get(15)?,
                        last_failure_reason: r.get(16)?,
                        last_failure_class: r.get(17)?,
                        started_at: r.get(18)?,
                        attempt_count: r.get(19)?,
                        current_attempt_id: r.get(20)?,
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
    /// C1 lifecycle additions.
    pub retry_count: i64,
    pub retry_policy: String,
    pub max_retries: i64,
    pub max_runtime_secs: Option<i64>,
    pub last_failure_reason: Option<String>,
    pub last_failure_class: Option<String>,
    pub started_at: Option<i64>,
    /// C2a attempt lineage. Cached pointers; the authoritative
    /// per-attempt timeline lives in `task_attempts` and is fetched
    /// via [`TaskStore::list_attempts`].
    pub attempt_count: i64,
    pub current_attempt_id: Option<i64>,
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

/// One execution attempt of a Task, returned by `task.attempts` and
/// folded into `task.get`'s rendered output. `attempt_num` is 1-based
/// per task; `attempt_id` is globally monotonic.
#[derive(Debug, Clone)]
pub struct AttemptView {
    pub attempt_id: i64,
    pub attempt_num: i64,
    pub started_at: i64,
    /// `None` while the attempt is still in flight (`status =
    /// 'running'`).
    pub finished_at: Option<i64>,
    /// One of `running` / `completed` / `failed` / `cancelled` /
    /// `interrupted`. Drift-resistant: the same vocabulary the
    /// `task.update` handler accepts.
    pub status: String,
    pub flow_id: Option<String>,
    pub flow_log_path: Option<String>,
    pub trace_id: Option<String>,
    pub error_kind: Option<i64>,
    pub error_cause: Option<String>,
    pub failure_class: Option<String>,
}

// ──────────────────────────── Capability registration ──────────────────────

/// Register the task capabilities on the dispatch bridge.
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
    {
        let s = store.clone();
        bridge.register(
            "task.recover",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_recover(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.attempts",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_attempts(&s, &ctx) }
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
    // `title|flow_template|params_json|owner_subject_id|retry_policy|max_retries|max_runtime_secs`.
    // Retry/runtime trailer is optional; callers can leave the suffix
    // off entirely or send empty slots. params_json that contains `|`
    // should be base64-encoded by the caller (SIMP-016).
    let parts: Vec<&str> = s.splitn(7, '|').collect();
    let title = parts.first().copied().unwrap_or("");
    let flow_template = parts.get(1).copied().unwrap_or("");
    let params_json = parts.get(2).copied().unwrap_or("");
    let owner = parts.get(3).copied().unwrap_or("");
    let retry_policy_str = parts.get(4).copied().unwrap_or("");
    let max_retries_str = parts.get(5).copied().unwrap_or("");
    let max_runtime_str = parts.get(6).copied().unwrap_or("");
    if title.is_empty() || flow_template.is_empty() {
        return invalid("task.create: `title` and `flow_template` are required".to_string());
    }
    let owner = if owner.is_empty() {
        ctx.caller.subject_id.to_string()
    } else {
        owner.to_string()
    };
    let retry_policy = if retry_policy_str.is_empty() {
        RetryPolicy::None
    } else {
        match RetryPolicy::parse(retry_policy_str) {
            Some(p) => p,
            None => return invalid(format!("task.create: bad retry_policy: {retry_policy_str}")),
        }
    };
    let max_retries: i64 = if max_retries_str.is_empty() {
        0
    } else {
        match max_retries_str.parse() {
            Ok(v) if v >= 0 => v,
            _ => return invalid(format!("task.create: bad max_retries: {max_retries_str}")),
        }
    };
    let max_runtime_secs: Option<i64> = if max_runtime_str.is_empty() {
        None
    } else {
        match max_runtime_str.parse::<i64>() {
            Ok(v) if v > 0 => Some(v),
            _ => {
                return invalid(format!(
                    "task.create: bad max_runtime_secs: {max_runtime_str}"
                ));
            }
        }
    };
    match store.create(
        title,
        flow_template,
        params_json,
        &owner,
        retry_policy,
        max_retries,
        max_runtime_secs,
    ) {
        Ok(id) => HandlerOutcome::Ok(id.into_bytes()),
        Err(e) => internal(format!("task.create: {e}")),
    }
}

fn handle_update(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.update utf8: {e}")),
    };
    // `task_id|status|result|flow_id|flow_log_path|error_kind|error_cause|failure_class|trace_id`.
    // The two trailers (failure_class, trace_id) are optional; older
    // callers that omit one or both keep working unchanged. trace_id
    // is only honored when the call opens a new attempt (status =
    // running, no open attempt); ignored otherwise.
    let parts: Vec<&str> = s.splitn(9, '|').collect();
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
    let failure_class_str = get(7);
    let trace_id_str = get(8);
    if let Some(fc) = failure_class_str
        && FailureClass::parse(fc).is_none()
    {
        return invalid(format!("task.update: bad failure_class: {fc}"));
    }
    if let Some(t) = trace_id_str
        && (t.len() != 32 || !t.chars().all(|c| c.is_ascii_hexdigit()))
    {
        return invalid(format!(
            "task.update: bad trace_id (want 32 hex chars): {t}"
        ));
    }
    match store.update_with_trace(
        task_id,
        status,
        result,
        flow_id,
        flow_log_path,
        error_kind,
        error_cause,
        failure_class_str,
        trace_id_str,
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

/// `task.attempts` — list every attempt of a task in chronological
/// order. Args: `task_id`. Returns one tab-delimited line per
/// attempt: `attempt_num\tstatus\tstarted_at\tfinished_at|-\tfailure_class|-\tflow_id|-`.
/// Empty body when the task has no attempts yet (created but never
/// transitioned to running).
fn handle_attempts(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.attempts utf8: {e}")),
    };
    if task_id.is_empty() {
        return invalid("task.attempts: task_id required".to_string());
    }
    match store.list_attempts(task_id) {
        Ok(rows) => {
            let mut buf = String::new();
            for a in rows {
                let finished = a
                    .finished_at
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "-".into());
                let class = a.failure_class.as_deref().unwrap_or("-");
                let flow = a.flow_id.as_deref().unwrap_or("-");
                buf.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\n",
                    a.attempt_num, a.status, a.started_at, finished, class, flow,
                ));
            }
            HandlerOutcome::Ok(buf.into_bytes())
        }
        Err(e) => internal(format!("task.attempts: {e}")),
    }
}

/// Operator-triggered recovery scan. Equivalent to the one the
/// coordinator runs at startup, but on-demand — useful when an
/// operator just set `max_runtime_secs` on a long-running task and
/// wants the scan to act now without restarting the node.
///
/// Args: empty (the scan reads `now` itself). Returns one line per
/// recovered task id, plus a trailing `recovered=N` line so callers
/// don't have to count.
fn handle_recover(store: &TaskStore, _ctx: &InvocationCtx) -> HandlerOutcome {
    let now = unix_secs();
    match store.recover_interrupted(now) {
        Ok(ids) => {
            let mut buf = String::with_capacity(ids.len() * 33 + 32);
            for id in &ids {
                buf.push_str(id);
                buf.push('\n');
            }
            buf.push_str(&format!("recovered={}\n", ids.len()));
            HandlerOutcome::Ok(buf.into_bytes())
        }
        Err(e) => internal(format!("task.recover: {e}")),
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
    let _ = writeln!(s, "retry_count={}", v.retry_count);
    let _ = writeln!(s, "retry_policy={}", v.retry_policy);
    let _ = writeln!(s, "max_retries={}", v.max_retries);
    if let Some(x) = v.max_runtime_secs {
        let _ = writeln!(s, "max_runtime_secs={}", x);
    }
    if let Some(x) = v.started_at {
        let _ = writeln!(s, "started_at={}", x);
    }
    if let Some(x) = v.last_failure_class.as_ref() {
        let _ = writeln!(s, "last_failure_class={}", x);
    }
    if let Some(x) = v.last_failure_reason.as_ref() {
        let _ = writeln!(s, "last_failure_reason={}", x);
    }
    let _ = writeln!(s, "attempt_count={}", v.attempt_count);
    if let Some(x) = v.current_attempt_id {
        let _ = writeln!(s, "current_attempt_id={}", x);
    }
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
        CREATE INDEX IF NOT EXISTS tasks_status ON tasks(status);

        CREATE TABLE IF NOT EXISTS task_events (
            event_id   INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id    TEXT    NOT NULL,
            ts         INTEGER NOT NULL,
            event_type TEXT    NOT NULL,
            payload    TEXT    NOT NULL,
            FOREIGN KEY (task_id) REFERENCES tasks(task_id)
        );
        CREATE INDEX IF NOT EXISTS task_events_task ON task_events(task_id, event_id);

        -- C2a: per-attempt execution records. The `tasks` row carries
        -- the cached "latest attempt" pointer for fast lookup; the
        -- authoritative per-attempt timeline lives here.
        CREATE TABLE IF NOT EXISTS task_attempts (
            attempt_id    INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id       TEXT    NOT NULL,
            attempt_num   INTEGER NOT NULL,
            started_at    INTEGER NOT NULL,
            finished_at   INTEGER,
            status        TEXT    NOT NULL,
            flow_id       TEXT,
            flow_log_path TEXT,
            trace_id      TEXT,
            error_kind    INTEGER,
            error_cause   TEXT,
            failure_class TEXT,
            FOREIGN KEY (task_id) REFERENCES tasks(task_id),
            UNIQUE (task_id, attempt_num)
        );
        CREATE INDEX IF NOT EXISTS task_attempts_by_task
            ON task_attempts(task_id, attempt_num);
        "#,
    )
    .map_err(CoordinatorError::Db)?;

    // C1: idempotent additive schema migration for the new lifecycle
    // columns. SQLite rejects ADD COLUMN of a duplicate name; we
    // intentionally ignore the resulting error so re-runs against an
    // already-migrated DB are a no-op. A proper migration framework
    // lands at Gate 2 along with the typed event payloads.
    let alters = [
        "ALTER TABLE tasks ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE tasks ADD COLUMN retry_policy TEXT NOT NULL DEFAULT 'none'",
        "ALTER TABLE tasks ADD COLUMN max_retries INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE tasks ADD COLUMN max_runtime_secs INTEGER",
        "ALTER TABLE tasks ADD COLUMN last_failure_reason TEXT",
        "ALTER TABLE tasks ADD COLUMN last_failure_class TEXT",
        "ALTER TABLE tasks ADD COLUMN started_at INTEGER",
        // C2a: cached pointer into task_attempts. NULL until the first
        // 'running' transition opens an attempt.
        "ALTER TABLE tasks ADD COLUMN attempt_count INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE tasks ADD COLUMN current_attempt_id INTEGER",
    ];
    for sql in alters {
        // Best-effort. The only error we expect is "duplicate column
        // name" on a re-init; any other error here is a schema bug.
        let _ = conn.execute(sql, []);
    }
    Ok(())
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

// ──────────────────────────── Attempt helpers (C2a) ─────────────────────────

/// Open a new attempt row if and only if the task has no open
/// attempt right now. Idempotent on the bridge side: a re-asserted
/// `running` status is a no-op at the attempt level.
///
/// Always called inside a transaction with the parent `update` (or
/// the `recover` retry-restart path) so observers cannot see the
/// cached `tasks.current_attempt_id` diverge from the attempts table.
fn open_attempt_if_needed(
    tx: &rusqlite::Transaction<'_>,
    task_id: &str,
    trace_id: Option<&str>,
    now: i64,
) -> Result<(), CoordinatorError> {
    let current: Option<(Option<i64>, i64)> = tx
        .query_row(
            "SELECT current_attempt_id, attempt_count FROM tasks WHERE task_id = ?1",
            params![task_id],
            |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(CoordinatorError::Db)?;
    let Some((current_id, count)) = current else {
        return Err(CoordinatorError::NotFound(task_id.to_string()));
    };
    if let Some(aid) = current_id {
        // If the current attempt is still open, leave it alone.
        let still_open: bool = tx
            .query_row(
                "SELECT finished_at IS NULL FROM task_attempts WHERE attempt_id = ?1",
                params![aid],
                |r| r.get(0),
            )
            .map_err(CoordinatorError::Db)?;
        if still_open {
            return Ok(());
        }
    }
    let next_num = count + 1;
    tx.execute(
        "INSERT INTO task_attempts
            (task_id, attempt_num, started_at, status, trace_id)
         VALUES (?1, ?2, ?3, 'running', ?4)",
        params![task_id, next_num, now, trace_id],
    )
    .map_err(CoordinatorError::Db)?;
    let new_id = tx.last_insert_rowid();
    tx.execute(
        "UPDATE tasks
         SET attempt_count = ?1,
             current_attempt_id = ?2
         WHERE task_id = ?3",
        params![next_num, new_id, task_id],
    )
    .map_err(CoordinatorError::Db)?;
    let payload = match trace_id {
        Some(t) => format!("attempt_id={new_id} attempt_num={next_num} trace_id={t}"),
        None => format!("attempt_id={new_id} attempt_num={next_num}"),
    };
    tx.execute(
        "INSERT INTO task_events (task_id, ts, event_type, payload)
         VALUES (?1, ?2, 'task.attempt_started', ?3)",
        params![task_id, now, payload],
    )
    .map_err(CoordinatorError::Db)?;
    Ok(())
}

/// Close the currently-open attempt row, if any, with the supplied
/// terminal outcome columns. Emits `task.attempt_finished`. Silently
/// no-ops when there is no open attempt — this preserves the pre-
/// C2a flow where a caller may go straight from `pending` to a
/// terminal status without an intervening `running` transition.
#[allow(clippy::too_many_arguments)]
fn close_open_attempt_if_any(
    tx: &rusqlite::Transaction<'_>,
    task_id: &str,
    new_status: &str,
    flow_id: Option<&str>,
    flow_log_path: Option<&str>,
    error_kind: Option<i64>,
    error_cause: Option<&str>,
    failure_class: Option<&str>,
    now: i64,
) -> Result<(), CoordinatorError> {
    let current_id: Option<i64> = tx
        .query_row(
            "SELECT current_attempt_id FROM tasks WHERE task_id = ?1",
            params![task_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(CoordinatorError::Db)?
        .flatten();
    let Some(aid) = current_id else {
        return Ok(());
    };
    let still_open: Option<bool> = tx
        .query_row(
            "SELECT finished_at IS NULL FROM task_attempts WHERE attempt_id = ?1",
            params![aid],
            |r| r.get(0),
        )
        .optional()
        .map_err(CoordinatorError::Db)?;
    if !matches!(still_open, Some(true)) {
        return Ok(());
    }
    tx.execute(
        "UPDATE task_attempts
         SET finished_at = ?1,
             status = ?2,
             flow_id = COALESCE(?3, flow_id),
             flow_log_path = COALESCE(?4, flow_log_path),
             error_kind = COALESCE(?5, error_kind),
             error_cause = COALESCE(?6, error_cause),
             failure_class = COALESCE(?7, failure_class)
         WHERE attempt_id = ?8",
        params![
            now,
            new_status,
            flow_id,
            flow_log_path,
            error_kind,
            error_cause,
            failure_class,
            aid,
        ],
    )
    .map_err(CoordinatorError::Db)?;
    let mut payload = format!("attempt_id={aid} status={new_status}");
    if let Some(fc) = failure_class {
        payload.push_str(&format!(" failure_class={fc}"));
    }
    tx.execute(
        "INSERT INTO task_events (task_id, ts, event_type, payload)
         VALUES (?1, ?2, 'task.attempt_finished', ?3)",
        params![task_id, now, payload],
    )
    .map_err(CoordinatorError::Db)?;
    Ok(())
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

    /// Test helper: create a task with the C1 defaults so we don't have
    /// to repeat the `RetryPolicy::None, 0, None` trailer at every call
    /// site.
    fn mk(s: &TaskStore, title: &str, flow: &str, params: &str, owner: &str) -> String {
        s.create(title, flow, params, owner, RetryPolicy::None, 0, None)
            .unwrap()
    }

    #[test]
    fn create_and_get_roundtrip() {
        let s = store();
        let tid = mk(&s, "demo task", "chat_template.sol", "{}", "owner-xyz");
        assert_eq!(tid.len(), 32);
        let v = s.get(&tid).unwrap().expect("present");
        assert_eq!(v.title, "demo task");
        assert_eq!(v.status, "pending");
        assert_eq!(v.flow_template, "chat_template.sol");
        assert_eq!(v.owner_subject_id, "owner-xyz");
        assert_eq!(v.events.len(), 0);
        assert_eq!(v.retry_count, 0);
        assert_eq!(v.retry_policy, "none");
        assert_eq!(v.max_retries, 0);
        assert!(v.max_runtime_secs.is_none());
        assert!(v.started_at.is_none());
        assert!(v.last_failure_class.is_none());
        assert!(v.last_failure_reason.is_none());
    }

    #[test]
    fn update_preserves_unset_fields() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(
            &tid,
            None,
            Some("the result body"),
            Some("flowabc"),
            Some("/tmp/x.log"),
            None,
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
        let tid = mk(&s, "t", "f", "{}", "o");
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
        let _ = mk(&s, "first", "f", "{}", "o");
        let second = mk(&s, "second", "f", "{}", "o");
        let _ = mk(&s, "third", "f", "{}", "o");
        std::thread::sleep(std::time::Duration::from_millis(1100));
        s.update(
            &second,
            Some("completed"),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let rows = s.list(10).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].title, "second");
    }

    #[test]
    fn update_unknown_task_is_invalid() {
        let s = store();
        match s.update(
            "deadbeef",
            Some("running"),
            None,
            None,
            None,
            None,
            None,
            None,
        ) {
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
        let tid = mk(&s, "render demo", "chat", "{\"a\":1}", "owner-1");
        s.append_event(&tid, "checkpoint", "step=1").unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        let rendered = render_task_view(&v);
        assert!(rendered.contains(&format!("task_id={tid}")));
        assert!(rendered.contains("status=pending"));
        assert!(rendered.contains("flow_template=chat"));
        assert!(rendered.contains("event_count=1"));
        assert!(rendered.contains("\"type\":\"checkpoint\""));
        assert!(rendered.contains("\"payload\":\"step=1\""));
        assert!(rendered.contains("retry_count=0"));
        assert!(rendered.contains("retry_policy=none"));
        assert!(rendered.contains("max_retries=0"));
    }

    // ── C1: lifecycle states + failure class + retry knobs ────────────

    #[test]
    fn retry_knobs_persist_on_create() {
        let s = store();
        let tid = s
            .create(
                "with knobs",
                "demo.sol",
                "{}",
                "alice",
                RetryPolicy::Bounded,
                3,
                Some(120),
            )
            .unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.retry_policy, "bounded");
        assert_eq!(v.max_retries, 3);
        assert_eq!(v.max_runtime_secs, Some(120));
        assert_eq!(v.retry_count, 0);
    }

    #[test]
    fn started_at_stamped_on_first_running_transition() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let first = s.get(&tid).unwrap().unwrap().started_at.unwrap();
        // Subsequent transitions back through `running` must not clobber
        // the original stamp — C1 recovery scan depends on this.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let again = s.get(&tid).unwrap().unwrap().started_at.unwrap();
        assert_eq!(first, again);
    }

    #[test]
    fn failure_class_and_reason_roundtrip() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            Some(error_kinds::TRANSPORT as i64),
            Some("dial failed"),
            Some("transient"),
        )
        .unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.status, "failed");
        assert_eq!(v.last_failure_class.as_deref(), Some("transient"));
        assert_eq!(v.last_failure_reason.as_deref(), Some("dial failed"));
        assert_eq!(v.error_cause.as_deref(), Some("dial failed"));
    }

    #[test]
    fn bump_retry_count_increments_and_returns_new_value() {
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, None)
            .unwrap();
        assert_eq!(s.bump_retry_count(&tid).unwrap(), 1);
        assert_eq!(s.bump_retry_count(&tid).unwrap(), 2);
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.retry_count, 2);
    }

    #[test]
    fn bump_retry_count_unknown_task_is_invalid() {
        let s = store();
        match s.bump_retry_count("nope") {
            Err(CoordinatorError::NotFound(id)) => assert_eq!(id, "nope"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn failure_class_from_kind_covers_known_kinds() {
        assert_eq!(
            FailureClass::from_kind(error_kinds::POLICY_DENIED),
            FailureClass::PolicyDenied
        );
        assert_eq!(
            FailureClass::from_kind(error_kinds::INVALID_ARGS),
            FailureClass::InvalidArgs
        );
        assert_eq!(
            FailureClass::from_kind(error_kinds::TIMEOUT),
            FailureClass::Timeout
        );
        assert_eq!(
            FailureClass::from_kind(error_kinds::TRANSPORT),
            FailureClass::Transient
        );
        assert_eq!(
            FailureClass::from_kind(error_kinds::RESPONDER_INTERNAL),
            FailureClass::Permanent
        );
        // Unknown kind defaults to Permanent so callers fail loudly
        // rather than silently retrying on something they don't model.
        assert_eq!(FailureClass::from_kind(9_999), FailureClass::Permanent);
    }

    // ── C1b: recovery scan ────────────────────────────────────────────

    #[test]
    fn recovery_scan_flips_overdue_running_tasks_to_interrupted() {
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::None, 0, Some(10))
            .unwrap();
        // started_at gets stamped to "now" when we transition to running.
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let started = s.get(&tid).unwrap().unwrap().started_at.unwrap();
        // Pretend a lot of wall clock has passed.
        let recovered = s.recover_interrupted(started + 60).unwrap();
        assert_eq!(recovered, vec![tid.clone()]);
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.status, "interrupted");
        assert_eq!(v.last_failure_class.as_deref(), Some("timeout"));
        assert!(
            v.last_failure_reason
                .as_deref()
                .unwrap()
                .contains("deadline_exceeded")
        );
        assert!(v.events.iter().any(|e| e.event_type == "task.interrupted"));
    }

    #[test]
    fn recovery_scan_leaves_tasks_without_deadline_alone() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let recovered = s.recover_interrupted(unix_secs() + 999_999).unwrap();
        assert!(recovered.is_empty());
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.status, "running");
    }

    #[test]
    fn recovery_scan_leaves_running_tasks_inside_deadline_alone() {
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::None, 0, Some(3600))
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let started = s.get(&tid).unwrap().unwrap().started_at.unwrap();
        let recovered = s.recover_interrupted(started + 30).unwrap();
        assert!(recovered.is_empty());
        assert_eq!(s.get(&tid).unwrap().unwrap().status, "running");
    }

    #[test]
    fn recovery_scan_is_idempotent() {
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::None, 0, Some(5))
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let started = s.get(&tid).unwrap().unwrap().started_at.unwrap();
        let first = s.recover_interrupted(started + 60).unwrap();
        assert_eq!(first.len(), 1);
        // Second pass finds nothing — row is no longer `running`.
        let second = s.recover_interrupted(started + 60).unwrap();
        assert!(second.is_empty());
        // And only one `task.interrupted` event was appended.
        let v = s.get(&tid).unwrap().unwrap();
        let n = v
            .events
            .iter()
            .filter(|e| e.event_type == "task.interrupted")
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn recovery_scan_skips_completed_and_failed_rows() {
        let s = store();
        let t_done = s
            .create("done", "f", "{}", "o", RetryPolicy::None, 0, Some(5))
            .unwrap();
        s.update(&t_done, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(
            &t_done,
            Some("completed"),
            Some("ok"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let started = s.get(&t_done).unwrap().unwrap().started_at.unwrap();
        let recovered = s.recover_interrupted(started + 60).unwrap();
        assert!(recovered.is_empty());
        assert_eq!(s.get(&t_done).unwrap().unwrap().status, "completed");
    }

    #[test]
    fn retry_policy_parse_roundtrip() {
        assert_eq!(RetryPolicy::parse("none"), Some(RetryPolicy::None));
        assert_eq!(RetryPolicy::parse("once"), Some(RetryPolicy::Once));
        assert_eq!(RetryPolicy::parse("bounded"), Some(RetryPolicy::Bounded));
        assert!(RetryPolicy::parse("forever").is_none());
        assert_eq!(RetryPolicy::None.as_str(), "none");
        assert_eq!(RetryPolicy::Bounded.as_str(), "bounded");
    }

    // ── C2a: per-attempt lineage ──────────────────────────────────────

    #[test]
    fn running_transition_opens_attempt_row() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        // No attempts yet.
        assert_eq!(s.list_attempts(&tid).unwrap().len(), 0);
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.attempt_count, 0);
        assert!(v.current_attempt_id.is_none());

        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.attempt_count, 1);
        let cur = v.current_attempt_id.expect("attempt opened");
        let attempts = s.list_attempts(&tid).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].attempt_id, cur);
        assert_eq!(attempts[0].attempt_num, 1);
        assert_eq!(attempts[0].status, "running");
        assert!(attempts[0].finished_at.is_none());
        // task.attempt_started event landed.
        assert!(
            v.events
                .iter()
                .any(|e| e.event_type == "task.attempt_started")
        );
    }

    #[test]
    fn running_transition_is_idempotent_at_attempt_level() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        // Still one attempt, not two.
        assert_eq!(s.list_attempts(&tid).unwrap().len(), 1);
    }

    #[test]
    fn terminal_transition_closes_open_attempt() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(
            &tid,
            Some("completed"),
            Some("ok"),
            Some("flowdeadbeef"),
            Some("/tmp/f.log"),
            None,
            None,
            None,
        )
        .unwrap();
        let attempts = s.list_attempts(&tid).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].status, "completed");
        assert!(attempts[0].finished_at.is_some());
        assert_eq!(attempts[0].flow_id.as_deref(), Some("flowdeadbeef"));
        assert_eq!(attempts[0].flow_log_path.as_deref(), Some("/tmp/f.log"));
        let v = s.get(&tid).unwrap().unwrap();
        assert!(
            v.events
                .iter()
                .any(|e| e.event_type == "task.attempt_finished")
        );
    }

    #[test]
    fn terminal_without_running_is_a_clean_noop_on_attempts() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        // Pre-C2a flow: pending -> completed straight, no running
        // transition. Attempts table stays empty, no error.
        s.update(
            &tid,
            Some("completed"),
            Some("ok"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(s.list_attempts(&tid).unwrap().len(), 0);
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.attempt_count, 0);
        assert_eq!(v.status, "completed");
    }

    #[test]
    fn retry_cycle_creates_second_attempt() {
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, None)
            .unwrap();
        // Attempt 1: running -> failed.
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            Some(error_kinds::TRANSPORT as i64),
            Some("net flake"),
            Some("transient"),
        )
        .unwrap();
        // Operator requests retry: status -> retrying. No attempt opens.
        s.update(&tid, Some("retrying"), None, None, None, None, None, None)
            .unwrap();
        let attempts = s.list_attempts(&tid).unwrap();
        assert_eq!(attempts.len(), 1, "retrying does not open a new attempt");
        // Bridge picks it up: running -> completed.
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(
            &tid,
            Some("completed"),
            Some("ok this time"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let attempts = s.list_attempts(&tid).unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].status, "failed");
        assert_eq!(attempts[0].failure_class.as_deref(), Some("transient"));
        assert_eq!(attempts[1].status, "completed");
        assert!(attempts[1].started_at >= attempts[0].finished_at.unwrap());
    }

    #[test]
    fn recovery_scan_closes_open_attempt_as_interrupted() {
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::None, 0, Some(5))
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let started = s.list_attempts(&tid).unwrap()[0].started_at;
        let recovered = s.recover_interrupted(started + 60).unwrap();
        assert_eq!(recovered.len(), 1);
        let attempts = s.list_attempts(&tid).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].status, "interrupted");
        assert_eq!(attempts[0].failure_class.as_deref(), Some("timeout"));
        assert!(attempts[0].finished_at.is_some());
        let v = s.get(&tid).unwrap().unwrap();
        assert!(
            v.events
                .iter()
                .any(|e| e.event_type == "task.attempt_finished"
                    && e.payload.contains("failure_class=timeout"))
        );
    }

    #[test]
    fn recovery_scan_uses_current_attempt_deadline_not_first_attempt() {
        // Regression guard for the C2a semantics shift: the scan
        // should key off the CURRENT attempt's started_at, not the
        // immutable task.started_at from the first attempt.
        //
        // Setup uses small (2s) deadline + 2.5s sleep so the test
        // doesn't depend on sub-second precision. We then check
        // `recover_interrupted(now)` for `now = second_started + 1`,
        // which is past attempt-1's 2-second deadline but inside
        // attempt-2's.
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, Some(2))
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let first_started = s.list_attempts(&tid).unwrap()[0].started_at;
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            Some("blip"),
            Some("transient"),
        )
        .unwrap();
        // Wait long enough that attempt 2's started_at is past
        // attempt 1's deadline (2s) by at least one whole second.
        std::thread::sleep(std::time::Duration::from_millis(2500));
        s.update(&tid, Some("retrying"), None, None, None, None, None, None)
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let second_started = s.list_attempts(&tid).unwrap()[1].started_at;
        // Sanity: attempt 2 started after attempt 1's deadline.
        assert!(second_started >= first_started + 2);
        // Choose `now` inside attempt-2's window but past attempt-1's.
        let now = second_started + 1;
        assert!(now > first_started + 2, "now past attempt-1 deadline");
        assert!(now < second_started + 2, "now inside attempt-2 deadline");
        let recovered = s.recover_interrupted(now).unwrap();
        assert!(
            recovered.is_empty(),
            "scan must use current attempt's deadline, not task.started_at"
        );
        assert_eq!(s.get(&tid).unwrap().unwrap().status, "running");
    }

    #[test]
    fn update_with_trace_persists_trace_id_on_open_attempt() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let trace_hex = "00112233445566778899aabbccddeeff";
        s.update_with_trace(
            &tid,
            Some("running"),
            None,
            None,
            None,
            None,
            None,
            None,
            Some(trace_hex),
        )
        .unwrap();
        let attempts = s.list_attempts(&tid).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].trace_id.as_deref(), Some(trace_hex));
        // Re-asserted running with a different trace_id leaves the
        // first attempt's trace_id untouched (no new attempt opened).
        s.update_with_trace(
            &tid,
            Some("running"),
            None,
            None,
            None,
            None,
            None,
            None,
            Some("ffffffffffffffffffffffffffffffff"),
        )
        .unwrap();
        let attempts = s.list_attempts(&tid).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].trace_id.as_deref(), Some(trace_hex));
    }
}
