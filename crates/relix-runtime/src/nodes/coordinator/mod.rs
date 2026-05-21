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
//! | `task.list`     | `` (empty) or `limit\|offset\|status` (limit default 50, all optional) | one `task_id\tstatus\ttitle\n` per line |
//! | `task.list_cursor` | `` (empty) or `limit\|status\|cursor` (cursor = `<updated_at>:<task_id>`; empty = first page) | rows + `next_cursor=<value>\n` trailer; stable under concurrent writes |
//! | `task.count`    | `` (empty) or `<status>` | `count=N\n` |
//! | `task.events`   | `task_id\|after_id\|limit\|type\|order` (after_id default 0, limit default 200, type empty = no filter, order in {asc, desc}, default asc) | one JSON event per line (`{"id":N,"ts":N,"type":"...","payload":"..."}`) |
//! | `task.recover`  | (empty) | one `task_id\n` per recovered task, then `recovered=N\n` |
//! | `task.attempts` | `task_id` | one `attempt_num\tstatus\tstarted_at\tfinished_at\|-\tfailure_class\|-\tflow_id\|-\n` per attempt |
//! | `task.retry`    | `task_id` | `accepted attempt=N of_budget=M\n` / `exhausted retry_count=N budget=M\n` / INVALID_ARGS with cause |
//! | `task.export`   | `task_id` | single JSON archival snapshot (`{schema_version, exported_at, task_id, task, attempts}`) |
//! | `task.compact_events` | `max_age_secs\|mode` (mode defaults to `dry-run`; only `dry-run` is shipped) | single JSON object `{mode, destructive:false, cutoff_ts, candidate_events, candidate_tasks, oldest_candidate_ts?, newest_candidate_ts?, by_task_status:{...}}` |
//! | `task.edges`    | `task_id` | one `edge_id\tedge_type\tattempt_id\|-\trelated_task_id\|-\trelated_attempt_id\|-\tspawned_by_event_id\|-\tcreated_at\n` per execution edge touching the task (as child or parent). Phase-1E: only `retried_from` emitted today. |
//! | `task.recent_edges` | `since_edge_id\|limit` (both optional; defaults 0/50) | newest-first cross-task edges: `edge_id\tedge_type\ttask_id\tattempt_id\|-\trelated_task_id\|-\trelated_attempt_id\|-\tspawned_by_event_id\|-\tcreated_at\n` per row. |
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

/// Direction for [`TaskStore::query_events`]. `Asc` is the
/// long-poll-friendly default (cursor advances through monotonic
/// `event_id`s); `Desc` is for "give me the last N events"
/// tail-queries operators use during interactive triage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventOrder {
    Asc,
    Desc,
}

impl EventOrder {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "asc" | "" => Some(Self::Asc),
            "desc" => Some(Self::Desc),
            _ => None,
        }
    }
}

/// Outcome of a [`TaskStore::request_retry`] call. The Coordinator
/// is honest about all three outcomes so the CLI can render them
/// without guessing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetryDecision {
    /// Retry allowed and applied: status flipped to `retrying`,
    /// `retry_count` bumped, `task.retry_requested` event emitted.
    Accepted { new_retry_count: i64, budget: i64 },
    /// Retry budget already exhausted. A `task.retry_exhausted`
    /// event is appended so the chronicle records the decision.
    Exhausted { retry_count: i64, budget: i64 },
    /// Retry refused for a reason other than budget exhaustion
    /// (status not failed/interrupted, retry_policy=none, etc.). No
    /// state change.
    Rejected { reason: String },
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

    /// Bump `retry_count` by one. Low-level primitive — does NOT
    /// validate retry policy or emit events. Prefer [`request_retry`]
    /// for the operator-facing flow. Returns the new count.
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

    /// Operator-initiated retry request. Validates the task is in a
    /// state where retry makes sense, validates the retry budget, and
    /// on success transitions the task to `retrying`, increments
    /// `retry_count`, emits a `task.retry_requested` event, and clears
    /// the transient `error_kind` / `error_cause` columns (the
    /// failure CLASS + REASON are preserved on the previous attempt
    /// row, and on `last_failure_class` / `last_failure_reason` for
    /// quick lookup).
    ///
    /// Does NOT open a new attempt — the next `running` transition
    /// does that. Does NOT actually re-run the flow; re-execution is
    /// owned by whoever runs the flow (bridge auto-retry is not
    /// wired today; operator typically runs `relix-cli flow-run`
    /// against the same flow_template + params).
    ///
    /// On exhaustion (retry_count >= max_retries) the task is left
    /// as-is and a `task.retry_exhausted` event is appended so the
    /// chronicle records the decision. Returns a [`RetryDecision`]
    /// describing what happened.
    pub fn request_retry(&self, task_id: &str) -> Result<RetryDecision, CoordinatorError> {
        let now = unix_secs();
        let mut conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let tx = conn.transaction().map_err(CoordinatorError::Db)?;
        let row: Option<(String, String, i64, i64, Option<String>)> = tx
            .query_row(
                "SELECT status, retry_policy, retry_count, max_retries, last_failure_class
                 FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(CoordinatorError::Db)?;
        let Some((status, retry_policy, retry_count, max_retries, last_class)) = row else {
            return Err(CoordinatorError::NotFound(task_id.to_string()));
        };

        // Only retry from terminal-bad states.
        match status.as_str() {
            "failed" | "interrupted" => {}
            other => {
                return Ok(RetryDecision::Rejected {
                    reason: format!("task is in status `{other}`, not failed/interrupted"),
                });
            }
        }

        if retry_policy == "none" {
            return Ok(RetryDecision::Rejected {
                reason: "retry_policy=none on this task".into(),
            });
        }
        let budget = match retry_policy.as_str() {
            "once" => 1,
            "bounded" => max_retries.max(0),
            _ => 0,
        };
        if retry_count >= budget {
            // Exhausted. Append an event so the chronicle is honest.
            let exhausted_legacy =
                format!("retry_count={retry_count} budget={budget} policy={retry_policy}");
            let exhausted_json = format!(
                r#"{{"retry_count":{retry_count},"budget":{budget},"policy":"{}"}}"#,
                json_escape(&retry_policy)
            );
            insert_typed_event(
                &tx,
                task_id,
                now,
                "task.retry_exhausted",
                &exhausted_legacy,
                None,
                None,
                Some(&exhausted_json),
            )?;
            tx.commit().map_err(CoordinatorError::Db)?;
            return Ok(RetryDecision::Exhausted {
                retry_count,
                budget,
            });
        }

        let new_count = retry_count + 1;
        tx.execute(
            "UPDATE tasks
             SET status = 'retrying',
                 retry_count = ?1,
                 updated_at = ?2,
                 error_kind = NULL,
                 error_cause = NULL
             WHERE task_id = ?3",
            params![new_count, now, task_id],
        )
        .map_err(CoordinatorError::Db)?;
        let prior = last_class.as_deref().unwrap_or("-");
        let requested_legacy = format!(
            "attempt={new_count} of_budget={budget} policy={retry_policy} prior_class={prior}",
        );
        let requested_json = format!(
            r#"{{"attempt":{new_count},"of_budget":{budget},"policy":"{}","prior_class":"{}"}}"#,
            json_escape(&retry_policy),
            json_escape(prior),
        );
        insert_typed_event(
            &tx,
            task_id,
            now,
            "task.retry_requested",
            &requested_legacy,
            None,
            None,
            Some(&requested_json),
        )?;
        tx.commit().map_err(CoordinatorError::Db)?;
        Ok(RetryDecision::Accepted {
            new_retry_count: new_count,
            budget,
        })
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
            let legacy_interrupt = format!(
                "started_at={started} max_runtime_secs={max} now={now_secs} reason=deadline_exceeded"
            );
            let interrupt_json = format!(
                r#"{{"started_at":{started},"max_runtime_secs":{max},"now":{now_secs},"reason":"deadline_exceeded"}}"#
            );
            insert_typed_event(
                &tx,
                &tid,
                now_secs,
                "task.interrupted",
                &legacy_interrupt,
                current_attempt,
                None,
                Some(&interrupt_json),
            )?;
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
                let finished_legacy =
                    format!("attempt_id={attempt_id} status=interrupted failure_class=timeout");
                let finished_json = format!(
                    r#"{{"attempt_id":{attempt_id},"status":"interrupted","failure_class":"timeout"}}"#
                );
                insert_typed_event(
                    &tx,
                    &tid,
                    now_secs,
                    "task.attempt_finished",
                    &finished_legacy,
                    Some(attempt_id),
                    None,
                    Some(&finished_json),
                )?;
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

    /// List every execution edge that touches `task_id` —
    /// either as the child (edges where `task_id = ?`) or as
    /// the parent (edges where `related_task_id = ?`).
    /// Returned oldest-first by `edge_id` so the chain
    /// reads chronologically.
    ///
    /// Phase-1E M38: today only the `retried_from` edge type
    /// is actively emitted (by `open_attempt_if_needed` when
    /// a retry opens a new attempt). Other edge types in the
    /// schema are reserved for runtime primitives that don't
    /// ship yet.
    pub fn list_edges_for_task(&self, task_id: &str) -> Result<Vec<TaskEdge>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT edge_id, task_id, attempt_id, edge_type,
                        related_task_id, related_attempt_id,
                        spawned_by_event_id, created_at
                 FROM task_edges
                 WHERE task_id = ?1 OR related_task_id = ?1
                 ORDER BY edge_id ASC",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![task_id], |r| {
                Ok(TaskEdge {
                    edge_id: r.get(0)?,
                    task_id: r.get(1)?,
                    attempt_id: r.get(2)?,
                    edge_type: r.get(3)?,
                    related_task_id: r.get(4)?,
                    related_attempt_id: r.get(5)?,
                    spawned_by_event_id: r.get(6)?,
                    created_at: r.get(7)?,
                })
            })
            .map_err(CoordinatorError::Db)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(CoordinatorError::Db)?);
        }
        Ok(out)
    }

    /// List the most recent execution edges across ALL tasks.
    /// Operators use this to spot patterns ("retry storm on
    /// task X") without per-task drill-in. Newest-first by
    /// `edge_id`. `since_edge_id` is a strict-greater-than
    /// cursor for incremental polling; pass 0 to read the
    /// most recent `limit` edges.
    ///
    /// Phase-1E M39: only retried_from is populated today —
    /// the function returns whatever edge_types the table
    /// holds.
    pub fn list_recent_edges(
        &self,
        since_edge_id: i64,
        limit: usize,
    ) -> Result<Vec<TaskEdge>, CoordinatorError> {
        let cap = limit.clamp(1, self.max_list);
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT edge_id, task_id, attempt_id, edge_type,
                        related_task_id, related_attempt_id,
                        spawned_by_event_id, created_at
                 FROM task_edges
                 WHERE edge_id > ?1
                 ORDER BY edge_id DESC
                 LIMIT ?2",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![since_edge_id, cap as i64], |r| {
                Ok(TaskEdge {
                    edge_id: r.get(0)?,
                    task_id: r.get(1)?,
                    attempt_id: r.get(2)?,
                    edge_type: r.get(3)?,
                    related_task_id: r.get(4)?,
                    related_attempt_id: r.get(5)?,
                    spawned_by_event_id: r.get(6)?,
                    created_at: r.get(7)?,
                })
            })
            .map_err(CoordinatorError::Db)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(CoordinatorError::Db)?);
        }
        Ok(out)
    }

    /// Fetch a slice of one Task's chronicle. Events with
    /// `event_id > after_id` are returned, oldest-first, capped at
    /// `limit` (clamped to `[1, max_list]` to share the same upper
    /// bound as `task.list`). Used by long-poll-style operator
    /// dashboards: read once with `after_id=0`, remember the largest
    /// id returned, poll again with that id to fetch only what's
    /// new.
    ///
    /// Returns `Ok(empty Vec)` when the task exists but has no new
    /// events; returns `Err(NotFound)` when the task doesn't exist
    /// so dashboards stop polling lost rows.
    pub fn list_events_after(
        &self,
        task_id: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<TaskEvent>, CoordinatorError> {
        self.query_events(task_id, after_id, limit, None, EventOrder::Asc)
    }

    /// Generalised event query. Same NotFound semantics as
    /// [`list_events_after`] and the same `[1, max_list]` cap on
    /// `limit`.
    ///
    /// - `type_filter`: non-empty exact-match on `event_type`. None
    ///   (or empty) returns every event.
    /// - `order`: `Asc` for the standard long-poll pattern
    ///   (oldest-first, cursor advances); `Desc` for "give me the
    ///   last N events" tail queries.
    pub fn query_events(
        &self,
        task_id: &str,
        after_id: i64,
        limit: usize,
        type_filter: Option<&str>,
        order: EventOrder,
    ) -> Result<Vec<TaskEvent>, CoordinatorError> {
        let cap = limit.clamp(1, self.max_list);
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
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
        // SQLite parameter binding doesn't permit substituting
        // identifiers (column / direction), so we materialise the
        // four (order × filter) variants as four static SQL
        // strings. All four hit the same task_events_task index.
        let sql_with_type = match order {
            EventOrder::Asc => {
                "SELECT event_id, ts, event_type, payload,
                        schema_version, attempt_id, trace_id, payload_json
                 FROM task_events
                 WHERE task_id = ?1 AND event_id > ?2 AND event_type = ?4
                 ORDER BY event_id ASC LIMIT ?3"
            }
            EventOrder::Desc => {
                "SELECT event_id, ts, event_type, payload,
                        schema_version, attempt_id, trace_id, payload_json
                 FROM task_events
                 WHERE task_id = ?1 AND event_id > ?2 AND event_type = ?4
                 ORDER BY event_id DESC LIMIT ?3"
            }
        };
        let sql_no_type = match order {
            EventOrder::Asc => {
                "SELECT event_id, ts, event_type, payload,
                        schema_version, attempt_id, trace_id, payload_json
                 FROM task_events
                 WHERE task_id = ?1 AND event_id > ?2
                 ORDER BY event_id ASC LIMIT ?3"
            }
            EventOrder::Desc => {
                "SELECT event_id, ts, event_type, payload,
                        schema_version, attempt_id, trace_id, payload_json
                 FROM task_events
                 WHERE task_id = ?1 AND event_id > ?2
                 ORDER BY event_id DESC LIMIT ?3"
            }
        };
        let map_row = |r: &rusqlite::Row<'_>| {
            Ok(TaskEvent {
                event_id: r.get(0)?,
                ts: r.get(1)?,
                event_type: r.get(2)?,
                payload: r.get(3)?,
                schema_version: r.get(4)?,
                attempt_id: r.get(5)?,
                trace_id: r.get(6)?,
                payload_json: r.get(7)?,
            })
        };
        let mut out = Vec::with_capacity(cap);
        match type_filter.filter(|s| !s.is_empty()) {
            Some(t) => {
                let mut stmt = conn.prepare(sql_with_type).map_err(CoordinatorError::Db)?;
                let rows = stmt
                    .query_map(params![task_id, after_id, cap as i64, t], map_row)
                    .map_err(CoordinatorError::Db)?;
                for r in rows {
                    out.push(r.map_err(CoordinatorError::Db)?);
                }
            }
            None => {
                let mut stmt = conn.prepare(sql_no_type).map_err(CoordinatorError::Db)?;
                let rows = stmt
                    .query_map(params![task_id, after_id, cap as i64], map_row)
                    .map_err(CoordinatorError::Db)?;
                for r in rows {
                    out.push(r.map_err(CoordinatorError::Db)?);
                }
            }
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

    /// Set or clear the operator-set investigation marker on
    /// a task (M62). When `marked` is true, stamps the marker
    /// with the current time + records the supplied reason and
    /// emits a `task.investigation_marked` chronicle event.
    /// When false, clears both columns and emits
    /// `task.investigation_cleared`.
    ///
    /// `reason` is optional; when present at mark time it's
    /// stored verbatim (cap [`MAX_OPERATOR_NOTE_LEN`]). At
    /// clear time the reason argument is ignored.
    ///
    /// Returns the new marker value (`Some(ts)` after a mark,
    /// `None` after a clear).
    pub fn set_investigation_marker(
        &self,
        task_id: &str,
        marked: bool,
        reason: Option<&str>,
        author_subject_id: &str,
    ) -> Result<Option<i64>, CoordinatorError> {
        let trimmed_reason = reason.map(|s| s.trim()).filter(|s| !s.is_empty());
        if let Some(r) = trimmed_reason
            && r.len() > MAX_OPERATOR_NOTE_LEN
        {
            return Err(CoordinatorError::Invalid(format!(
                "task.mark_investigation: reason exceeds {MAX_OPERATOR_NOTE_LEN} bytes (got {})",
                r.len()
            )));
        }
        let now = unix_secs();
        let mut conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
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
        let tx = conn.transaction().map_err(CoordinatorError::Db)?;
        if marked {
            tx.execute(
                "UPDATE tasks
                   SET investigation_marked_at = ?2,
                       investigation_reason = ?3,
                       updated_at = ?2
                 WHERE task_id = ?1",
                params![task_id, now, trimmed_reason],
            )
            .map_err(CoordinatorError::Db)?;
            let payload_json = serde_json::json!({
                "marked": true,
                "reason": trimmed_reason,
                "author": author_subject_id,
            })
            .to_string();
            let legacy = match trimmed_reason {
                Some(r) => format!("marked · {r}"),
                None => "marked".to_string(),
            };
            insert_typed_event(
                &tx,
                task_id,
                now,
                "task.investigation_marked",
                &legacy,
                None,
                None,
                Some(&payload_json),
            )?;
            tx.commit().map_err(CoordinatorError::Db)?;
            Ok(Some(now))
        } else {
            tx.execute(
                "UPDATE tasks
                   SET investigation_marked_at = NULL,
                       investigation_reason = NULL,
                       updated_at = ?2
                 WHERE task_id = ?1",
                params![task_id, now],
            )
            .map_err(CoordinatorError::Db)?;
            let payload_json = serde_json::json!({
                "marked": false,
                "author": author_subject_id,
            })
            .to_string();
            insert_typed_event(
                &tx,
                task_id,
                now,
                "task.investigation_cleared",
                "cleared",
                None,
                None,
                Some(&payload_json),
            )?;
            tx.commit().map_err(CoordinatorError::Db)?;
            Ok(None)
        }
    }

    /// Append an operator-authored note as a structured
    /// `task.operator_note` chronicle event (M60). The note
    /// becomes part of the immutable task history and is
    /// surfaced alongside runtime events in any
    /// `task.events` / `task.export` consumer.
    ///
    /// `note` must be non-empty after trimming and is capped
    /// at `MAX_OPERATOR_NOTE_LEN` bytes; longer text is
    /// rejected with `Invalid` rather than silently truncated
    /// (operators should know).
    ///
    /// `author_subject_id` is the verified caller's
    /// subject_id — the bridge passes through `ctx.caller`,
    /// so the recorded author matches the admission-audit
    /// caller for the same RPC.
    ///
    /// Returns the new event_id so callers can deep-link
    /// into the chronicle.
    pub fn append_operator_note(
        &self,
        task_id: &str,
        note: &str,
        author_subject_id: &str,
    ) -> Result<i64, CoordinatorError> {
        let trimmed = note.trim();
        if trimmed.is_empty() {
            return Err(CoordinatorError::Invalid(
                "task.note: note text required (non-empty after trim)".to_string(),
            ));
        }
        if trimmed.len() > MAX_OPERATOR_NOTE_LEN {
            return Err(CoordinatorError::Invalid(format!(
                "task.note: note exceeds {MAX_OPERATOR_NOTE_LEN} bytes (got {})",
                trimmed.len()
            )));
        }
        let now = unix_secs();
        let mut conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
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
        // Structured envelope. Author + ts let the dashboard
        // render notes consistently with retry/cancel events.
        // The legacy `payload` string carries the trimmed note
        // verbatim so older grep-driven CLIs see the text;
        // payload_json carries the typed envelope.
        let payload_json = serde_json::json!({
            "note": trimmed,
            "author": author_subject_id,
        })
        .to_string();
        let tx = conn.transaction().map_err(CoordinatorError::Db)?;
        insert_typed_event(
            &tx,
            task_id,
            now,
            "task.operator_note",
            trimmed,
            None,
            None,
            Some(&payload_json),
        )?;
        let event_id = tx.last_insert_rowid();
        tx.commit().map_err(CoordinatorError::Db)?;
        Ok(event_id)
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
                        attempt_count, current_attempt_id,
                        investigation_marked_at, investigation_reason
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
                        investigation_marked_at: r.get(21)?,
                        investigation_reason: r.get(22)?,
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
                "SELECT event_id, ts, event_type, payload,
                        schema_version, attempt_id, trace_id, payload_json
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
                    schema_version: r.get(4)?,
                    attempt_id: r.get(5)?,
                    trace_id: r.get(6)?,
                    payload_json: r.get(7)?,
                })
            })
            .map_err(CoordinatorError::Db)?;
        for r in rows {
            view.events.push(r.map_err(CoordinatorError::Db)?);
        }
        Ok(Some(view))
    }

    /// Most-recently-updated tasks first, capped at `min(limit, max_list)`.
    /// Equivalent to `list_paginated(limit, 0, None)`.
    pub fn list(&self, limit: usize) -> Result<Vec<TaskSummary>, CoordinatorError> {
        self.list_paginated(limit, 0, None)
    }

    /// Most-recently-updated tasks first with offset-based pagination
    /// and optional server-side status filter. Returns at most
    /// `min(limit, max_list)` rows.
    ///
    /// `offset` skips the first N rows of the (filtered) ordering.
    /// Using offset is the operator-simple choice; cursor-based
    /// pagination is a follow-up if `tasks_updated` index growth
    /// becomes a measurable concern.
    ///
    /// `status_filter`, when set, narrows to rows whose `status`
    /// column matches exactly. Backed by the `tasks_status` index
    /// (added in C1a).
    pub fn list_paginated(
        &self,
        limit: usize,
        offset: usize,
        status_filter: Option<&str>,
    ) -> Result<Vec<TaskSummary>, CoordinatorError> {
        let cap = limit.clamp(1, self.max_list);
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let (sql, has_filter) = match status_filter {
            Some(_) => (
                "SELECT task_id, title, status, updated_at
                 FROM tasks WHERE status = ?3
                 ORDER BY updated_at DESC LIMIT ?1 OFFSET ?2",
                true,
            ),
            None => (
                "SELECT task_id, title, status, updated_at
                 FROM tasks ORDER BY updated_at DESC LIMIT ?1 OFFSET ?2",
                false,
            ),
        };
        let mut stmt = conn.prepare(sql).map_err(CoordinatorError::Db)?;
        let map_row = |r: &rusqlite::Row<'_>| {
            Ok(TaskSummary {
                task_id: r.get(0)?,
                title: r.get(1)?,
                status: r.get(2)?,
                updated_at: r.get(3)?,
                // The list_paginated SQL doesn't SELECT the
                // investigation column (the older list shape
                // never carried it). Set to None — clients
                // wanting the marker use list_cursor instead.
                investigation_marked_at: None,
            })
        };
        let mut out = Vec::with_capacity(cap);
        if has_filter {
            let rows = stmt
                .query_map(
                    params![cap as i64, offset as i64, status_filter.unwrap()],
                    map_row,
                )
                .map_err(CoordinatorError::Db)?;
            for r in rows {
                out.push(r.map_err(CoordinatorError::Db)?);
            }
        } else {
            let rows = stmt
                .query_map(params![cap as i64, offset as i64], map_row)
                .map_err(CoordinatorError::Db)?;
            for r in rows {
                out.push(r.map_err(CoordinatorError::Db)?);
            }
        }
        Ok(out)
    }

    /// Cursor-based pagination over the same `tasks` ordering as
    /// `list_paginated` (most-recently-updated first), but stable
    /// under concurrent inserts and updates. The cursor is the
    /// `(updated_at, task_id)` of the last row of the prior page;
    /// rows with the same `updated_at` are tie-broken by
    /// `task_id DESC` so two snapshots taken during a write burst
    /// never see the same row twice on adjacent pages and never
    /// silently skip one.
    ///
    /// `cursor = None` returns the first page; subsequent calls
    /// pass the `next_cursor` from the previous response. Empty
    /// `items` means the cursor has walked off the end (or the
    /// filter has no matches).
    ///
    /// `limit` is clamped to `[1, max_list]`.
    ///
    /// `status_filter` matches exactly when set; uses the existing
    /// `tasks_status` index.
    pub fn list_cursor(
        &self,
        cursor: Option<TaskCursor>,
        limit: usize,
        status_filter: Option<&str>,
    ) -> Result<TaskPage, CoordinatorError> {
        let cap = limit.clamp(1, self.max_list);
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        // Materialise four SQL strings rather than runtime-
        // substituting identifiers. Same pattern as query_events.
        let (sql, args): (&str, Vec<rusqlite::types::Value>) =
            match (cursor.as_ref(), status_filter) {
                (None, None) => (
                    "SELECT task_id, title, status, updated_at, investigation_marked_at
                     FROM tasks
                     ORDER BY updated_at DESC, task_id DESC
                     LIMIT ?1",
                    vec![(cap as i64).into()],
                ),
                (None, Some(s)) => (
                    "SELECT task_id, title, status, updated_at, investigation_marked_at
                     FROM tasks
                     WHERE status = ?2
                     ORDER BY updated_at DESC, task_id DESC
                     LIMIT ?1",
                    vec![(cap as i64).into(), s.to_string().into()],
                ),
                (Some(c), None) => (
                    "SELECT task_id, title, status, updated_at, investigation_marked_at
                     FROM tasks
                     WHERE (updated_at < ?2)
                        OR (updated_at = ?2 AND task_id < ?3)
                     ORDER BY updated_at DESC, task_id DESC
                     LIMIT ?1",
                    vec![
                        (cap as i64).into(),
                        c.updated_at.into(),
                        c.task_id.clone().into(),
                    ],
                ),
                (Some(c), Some(s)) => (
                    "SELECT task_id, title, status, updated_at, investigation_marked_at
                     FROM tasks
                     WHERE status = ?4
                       AND ((updated_at < ?2)
                            OR (updated_at = ?2 AND task_id < ?3))
                     ORDER BY updated_at DESC, task_id DESC
                     LIMIT ?1",
                    vec![
                        (cap as i64).into(),
                        c.updated_at.into(),
                        c.task_id.clone().into(),
                        s.to_string().into(),
                    ],
                ),
            };
        let mut stmt = conn.prepare(sql).map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok(TaskSummary {
                    task_id: r.get(0)?,
                    title: r.get(1)?,
                    status: r.get(2)?,
                    updated_at: r.get(3)?,
                    investigation_marked_at: r.get(4)?,
                })
            })
            .map_err(CoordinatorError::Db)?;
        let mut items = Vec::with_capacity(cap);
        for r in rows {
            items.push(r.map_err(CoordinatorError::Db)?);
        }
        let next_cursor = items.last().map(|r| TaskCursor {
            updated_at: r.updated_at,
            task_id: r.task_id.clone(),
        });
        Ok(TaskPage { items, next_cursor })
    }

    /// Export one Task's full archival artifact: header columns,
    /// every attempt row, every chronicle event in a single
    /// snapshot. Per `docs/chronicle-retention.md` this is the
    /// "save-before-delete" path the operator runs before any
    /// destructive deletion.
    ///
    /// Returns `Err(NotFound)` when the task doesn't exist.
    pub fn export_task(&self, task_id: &str) -> Result<TaskExport, CoordinatorError> {
        let view = self
            .get(task_id)?
            .ok_or_else(|| CoordinatorError::NotFound(task_id.to_string()))?;
        let attempts = self.list_attempts(task_id)?;
        Ok(TaskExport {
            schema_version: 1,
            exported_at: unix_secs(),
            task_id: view.task_id.clone(),
            view,
            attempts,
        })
    }

    /// Count `task_events` rows that *would* be deleted by a
    /// max-age retention pass — without deleting anything.
    ///
    /// `cutoff_ts` is the wall-clock unix-seconds threshold:
    /// events with `ts < cutoff_ts` are candidates. The query
    /// honours the chronicle-retention design's R5 invariant
    /// (only events belonging to terminal-state tasks are
    /// candidates), so callers may safely take the returned
    /// counts as ground truth for the deletion that Step 3
    /// would do.
    ///
    /// Returns counts grouped by parent task status so operators
    /// can see at a glance which terminal cohort dominates.
    pub fn count_compact_candidates(
        &self,
        cutoff_ts: i64,
    ) -> Result<CompactDryRun, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        // Aggregate counts in one round-trip: total, distinct task
        // count, oldest + newest ts within the candidate set.
        let (candidate_events, candidate_tasks, oldest_ts, newest_ts): (
            i64,
            i64,
            Option<i64>,
            Option<i64>,
        ) = conn
            .query_row(
                "SELECT COUNT(*),
                        COUNT(DISTINCT te.task_id),
                        MIN(te.ts),
                        MAX(te.ts)
                 FROM task_events te
                 JOIN tasks t ON t.task_id = te.task_id
                 WHERE te.ts < ?1
                   AND t.status IN ('completed', 'failed', 'cancelled', 'interrupted')",
                params![cutoff_ts],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .map_err(CoordinatorError::Db)?;
        // Per-status breakdown so operators see which terminal
        // cohort dominates the candidate set.
        let mut stmt = conn
            .prepare(
                "SELECT t.status, COUNT(*)
                 FROM task_events te
                 JOIN tasks t ON t.task_id = te.task_id
                 WHERE te.ts < ?1
                   AND t.status IN ('completed', 'failed', 'cancelled', 'interrupted')
                 GROUP BY t.status",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![cutoff_ts], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(CoordinatorError::Db)?;
        let mut by_task_status: Vec<(String, i64)> = Vec::new();
        for r in rows {
            by_task_status.push(r.map_err(CoordinatorError::Db)?);
        }
        // Sort for stable JSON output ordering — alphabetical so
        // tests and dashboards see a predictable shape.
        by_task_status.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(CompactDryRun {
            cutoff_ts,
            candidate_events,
            candidate_tasks,
            oldest_candidate_ts: oldest_ts,
            newest_candidate_ts: newest_ts,
            by_task_status,
        })
    }

    /// Total task count, optionally filtered by status. Drives
    /// pagination "total" hints for operator tooling that wants to
    /// render "N of M" without walking every page.
    pub fn count(&self, status_filter: Option<&str>) -> Result<i64, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let n: i64 = match status_filter {
            Some(s) => conn
                .query_row(
                    "SELECT COUNT(*) FROM tasks WHERE status = ?1",
                    params![s],
                    |r| r.get(0),
                )
                .map_err(CoordinatorError::Db)?,
            None => conn
                .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
                .map_err(CoordinatorError::Db)?,
        };
        Ok(n)
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
    /// M62: operator investigation marker. `Some(ts)` when the
    /// operator most recently called `task.mark_investigation`
    /// with `marked=true`; `None` when never marked or last
    /// cleared. The chronicle preserves the full toggle history
    /// via `task.investigation_marked` / `task.investigation_cleared`
    /// events.
    pub investigation_marked_at: Option<i64>,
    /// M62: operator-supplied short reason captured at the most
    /// recent mark. Cleared to `None` when the marker is
    /// cleared.
    pub investigation_reason: Option<String>,
    pub events: Vec<TaskEvent>,
}

/// One event appended via `task.event`.
///
/// Schema versioning (S2):
/// - `schema_version = 0`: legacy. Only `payload` (free string)
///   is populated. Operator-defined events via `task.event` stay
///   here.
/// - `schema_version = 1`: structured envelope. `attempt_id` and
///   `trace_id` are filled when known; `payload_json` carries
///   typed event data (still optional). Runtime emitters
///   (attempt open/close, recovery scan, retry request) use this
///   form. Legacy `payload` is also populated for back-compat
///   with old renderers.
///
/// All four new fields are `Option<_>` so older serialised
/// representations parse cleanly without bumping the type.
#[derive(Debug, Clone)]
pub struct TaskEvent {
    pub event_id: i64,
    pub ts: i64,
    pub event_type: String,
    pub payload: String,
    /// S2: 0 = legacy, 1 = structured envelope.
    pub schema_version: i64,
    /// S2: present on structured events that belong to an attempt.
    pub attempt_id: Option<i64>,
    /// S2: present on structured events emitted within a traced
    /// flow.
    pub trace_id: Option<String>,
    /// S2: optional typed payload as a JSON string. Free-form;
    /// the schema per event_type is documented in
    /// `docs/event-contract.md`.
    pub payload_json: Option<String>,
}

/// Compact Task representation returned by `task.list`.
#[derive(Debug, Clone)]
pub struct TaskSummary {
    pub task_id: String,
    pub title: String,
    pub status: String,
    pub updated_at: i64,
    /// M63: surfaced on list output so the dashboard can render
    /// a badge per row without a per-task drill-in. `None`
    /// when the marker is unset.
    pub investigation_marked_at: Option<i64>,
}

/// Cursor for [`TaskStore::list_cursor`]. Encodes the
/// `(updated_at, task_id)` of the last row of the prior page so
/// subsequent pages skip exactly past it. Wire-serialised as
/// `<updated_at>:<task_id>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskCursor {
    pub updated_at: i64,
    pub task_id: String,
}

impl TaskCursor {
    /// Encode for the wire (`<updated_at>:<task_id>`).
    pub fn encode(&self) -> String {
        format!("{}:{}", self.updated_at, self.task_id)
    }

    /// Parse the wire form. Returns `None` on malformed input;
    /// callers treat that as "start from the beginning" rather
    /// than failing the request, which keeps polling dashboards
    /// resilient to corrupted cursor state.
    pub fn parse(s: &str) -> Option<Self> {
        let (ts, tid) = s.split_once(':')?;
        let updated_at = ts.parse().ok()?;
        if tid.is_empty() {
            return None;
        }
        Some(Self {
            updated_at,
            task_id: tid.to_string(),
        })
    }
}

/// One page of cursor-paginated tasks. `next_cursor = None` only
/// when the page itself was empty; callers know the cursor walked
/// off the end by getting back `TaskPage { items: [], .. }` on the
/// next call.
#[derive(Debug, Clone)]
pub struct TaskPage {
    pub items: Vec<TaskSummary>,
    pub next_cursor: Option<TaskCursor>,
}

/// Operator's "save-before-delete" archival snapshot of one
/// Task. Returned by [`TaskStore::export_task`] and rendered by
/// the `task.export` capability handler as a single JSON object.
///
/// The shape is intentionally additive — older consumers
/// reading the inner `view` + `attempts` arrays keep working
/// even if future fields are added at the export envelope
/// level. `schema_version` is the kill-switch when the inner
/// shapes ever bump in a breaking way.
#[derive(Debug, Clone)]
pub struct TaskExport {
    /// Export-envelope schema version. Currently 1.
    pub schema_version: u32,
    /// Unix seconds at which the snapshot was taken.
    pub exported_at: i64,
    /// Convenience — same as `view.task_id`.
    pub task_id: String,
    pub view: TaskView,
    pub attempts: Vec<AttemptView>,
}

/// Result of a `task.compact_events` dry-run pass: the set of
/// `task_events` rows that *would* be deleted under the supplied
/// `cutoff_ts` policy (and the chronicle-retention design's R5
/// terminal-state invariant), aggregated by parent task status.
///
/// This is non-destructive — no rows are removed; the type
/// exists to give operators a clear picture before any Step 3
/// bounded-delete capability lands.
#[derive(Debug, Clone)]
pub struct CompactDryRun {
    /// Wall-clock unix seconds. Events with `ts < cutoff_ts`
    /// were counted as candidates.
    pub cutoff_ts: i64,
    /// Total `task_events` rows that match the policy.
    pub candidate_events: i64,
    /// Distinct tasks that contribute at least one candidate
    /// event. Always ≤ `candidate_events`.
    pub candidate_tasks: i64,
    /// `min(ts)` over the candidate set. `None` when
    /// `candidate_events == 0`.
    pub oldest_candidate_ts: Option<i64>,
    /// `max(ts)` over the candidate set. `None` when
    /// `candidate_events == 0`.
    pub newest_candidate_ts: Option<i64>,
    /// Per-terminal-status breakdown, sorted alphabetically by
    /// status for stable rendering. Statuses with zero
    /// candidates do not appear.
    pub by_task_status: Vec<(String, i64)>,
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

/// One row from `task_edges`. An execution edge that originated
/// from a recorded runtime action — never synthesised. See the
/// `edge_type` taxonomy in the `task_edges` schema docblock.
#[derive(Debug, Clone)]
pub struct TaskEdge {
    pub edge_id: i64,
    pub task_id: String,
    /// Attempt on the *child / current* side of the edge. None
    /// when the edge is task-scoped rather than attempt-scoped
    /// (no edge type uses this today; reserved for future
    /// task-spawned-by-task primitives).
    pub attempt_id: Option<i64>,
    /// One of the documented edge_type vocabulary. Only
    /// `retried_from` has a shipped emitter today.
    pub edge_type: String,
    /// Task on the *parent / dependency* side. For
    /// `retried_from` this equals `task_id` (same task, just
    /// a prior attempt). None for edge types that don't
    /// reference another task.
    pub related_task_id: Option<String>,
    /// Attempt on the *parent / dependency* side. For
    /// `retried_from` this is the prior attempt's id.
    pub related_attempt_id: Option<i64>,
    /// Chronicle event_id that triggered the edge. For
    /// `retried_from` this is the `task.retry_requested`
    /// event. None when the trigger isn't in the chronicle
    /// (legacy edges, runtime emit gaps).
    pub spawned_by_event_id: Option<i64>,
    pub created_at: i64,
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
    {
        let s = store.clone();
        bridge.register(
            "task.retry",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_retry(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.count",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_count(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.events",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_events(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.list_cursor",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_list_cursor(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.export",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_export(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.compact_events",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_compact_events(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.edges",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_edges(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.recent_edges",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_recent_edges(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.note",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_note(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.mark_investigation",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_mark_investigation(&s, &ctx) }
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
    // Wire format: `<limit>|<offset>|<status>`. All three optional.
    // Empty body == limit=50, offset=0, no status filter. Callers
    // that pass just `<N>` keep working (offset defaults to 0, status
    // to none). New callers can paginate via `100|200|failed`.
    let parts: Vec<&str> = s.splitn(3, '|').collect();
    let limit: usize = parts
        .first()
        .copied()
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let offset: usize = parts
        .get(1)
        .copied()
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let status_filter = parts.get(2).copied().filter(|v| !v.is_empty());
    match store.list_paginated(limit, offset, status_filter) {
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

/// `task.events` — incremental chronicle fetch for one task. Arg
/// shape: `task_id|after_id|limit|type|order`. `after_id` defaults
/// to 0 (read from the beginning); `limit` defaults to 200
/// (clamped to `[coordinator] max_list`). `type`, when non-empty,
/// is an exact-match event_type filter. `order` is `asc` (default;
/// the long-poll cursor pattern) or `desc` (newest-first; for
/// "last N events" tail queries).
///
/// Returns one JSON object per event, one per line:
/// `{"id":N,"ts":N,"type":"...","payload":"..."}`. Empty body when
/// the task has no matching events. Returns INVALID_ARGS on a
/// malformed task id or unknown order; `not found` if the task
/// doesn't exist (so polling dashboards drop the row).
fn handle_events(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.events utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(5, '|').collect();
    let task_id = parts.first().copied().unwrap_or("").trim();
    if task_id.is_empty() {
        return invalid("task.events: task_id required".to_string());
    }
    let after_id: i64 = parts
        .get(1)
        .copied()
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let limit: usize = parts
        .get(2)
        .copied()
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let type_filter = parts.get(3).copied().filter(|v| !v.is_empty());
    let order_str = parts.get(4).copied().unwrap_or("");
    let order = match EventOrder::parse(order_str) {
        Some(o) => o,
        None => return invalid(format!("task.events: bad order: {order_str}")),
    };
    match store.query_events(task_id, after_id, limit, type_filter, order) {
        Ok(events) => {
            let mut buf = String::with_capacity(events.len() * 128);
            for ev in &events {
                buf.push_str(&render_event_json(ev));
                buf.push('\n');
            }
            HandlerOutcome::Ok(buf.into_bytes())
        }
        Err(CoordinatorError::NotFound(id)) => invalid(format!("task.events: not found: {id}")),
        Err(e) => internal(format!("task.events: {e}")),
    }
}

/// `task.export` — archival snapshot of one task.
///
/// Args: `task_id` (32 hex). Returns one JSON object:
///
/// ```text
/// {
///   "schema_version": 1,
///   "exported_at":    1700000000,
///   "task_id":        "...",
///   "task":           { header: {...}, events: [...] },
///   "attempts":       [...]
/// }
/// ```
///
/// This is the operator's "save-before-delete" snapshot that
/// the chronicle-retention design requires before any
/// destructive deletion lands. Compact form (single JSON
/// object, not line-delimited) so it's directly archivable
/// to a file.
fn handle_export(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.export utf8: {e}")),
    };
    if task_id.is_empty() {
        return invalid("task.export: task_id required".to_string());
    }
    match store.export_task(task_id) {
        Ok(export) => HandlerOutcome::Ok(render_task_export(&export).into_bytes()),
        Err(CoordinatorError::NotFound(id)) => invalid(format!("task.export: not found: {id}")),
        Err(e) => internal(format!("task.export: {e}")),
    }
}

/// Hand-built JSON for one `TaskExport`. Same approach as the
/// other Coordinator renderers — no serde_json dependency on
/// the runtime crate.
#[allow(unused_assignments)] // `first` is mutated by macros below; final write is by design
fn render_task_export(e: &TaskExport) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(2048);
    let _ = write!(
        s,
        r#"{{"schema_version":{},"exported_at":{},"task_id":"{}","task":{{"#,
        e.schema_version,
        e.exported_at,
        json_escape(&e.task_id),
    );
    // Header fields. Build from the same struct fields the
    // existing renderer uses so any new column added there
    // surfaces here too.
    let v = &e.view;
    let mut first = true;
    macro_rules! push_str_field {
        ($key:expr, $val:expr) => {{
            if !first {
                s.push(',');
            }
            first = false;
            let _ = write!(s, r#""{}":"{}""#, $key, json_escape(&$val));
        }};
    }
    macro_rules! push_int_field {
        ($key:expr, $val:expr) => {{
            if !first {
                s.push(',');
            }
            first = false;
            let _ = write!(s, r#""{}":{}"#, $key, $val);
        }};
    }
    macro_rules! push_opt_str_field {
        ($key:expr, $val:expr) => {
            if let Some(x) = $val.as_ref() {
                if !first {
                    s.push(',');
                }
                first = false;
                let _ = write!(s, r#""{}":"{}""#, $key, json_escape(x));
            }
        };
    }
    macro_rules! push_opt_int_field {
        ($key:expr, $val:expr) => {
            if let Some(x) = $val {
                if !first {
                    s.push(',');
                }
                first = false;
                let _ = write!(s, r#""{}":{}"#, $key, x);
            }
        };
    }
    push_str_field!("title", v.title);
    push_str_field!("status", v.status);
    push_str_field!("owner_subject_id", v.owner_subject_id);
    push_str_field!("flow_template", v.flow_template);
    push_str_field!("params_json", v.params_json);
    push_opt_str_field!("latest_result", v.latest_result);
    push_opt_str_field!("latest_flow_id", v.latest_flow_id);
    push_opt_str_field!("latest_flow_log_path", v.latest_flow_log_path);
    push_opt_int_field!("error_kind", v.error_kind);
    push_opt_str_field!("error_cause", v.error_cause);
    push_int_field!("created_at", v.created_at);
    push_int_field!("updated_at", v.updated_at);
    push_int_field!("retry_count", v.retry_count);
    push_str_field!("retry_policy", v.retry_policy);
    push_int_field!("max_retries", v.max_retries);
    push_opt_int_field!("max_runtime_secs", v.max_runtime_secs);
    push_opt_str_field!("last_failure_reason", v.last_failure_reason);
    push_opt_str_field!("last_failure_class", v.last_failure_class);
    push_opt_int_field!("started_at", v.started_at);
    push_int_field!("attempt_count", v.attempt_count);
    push_opt_int_field!("current_attempt_id", v.current_attempt_id);
    push_opt_int_field!("investigation_marked_at", v.investigation_marked_at);
    push_opt_str_field!("investigation_reason", v.investigation_reason);
    s.push_str(r#","events":["#);
    for (i, ev) in v.events.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&render_event_json(ev));
    }
    s.push_str(r#"]},"attempts":["#);
    for (i, a) in e.attempts.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(
            s,
            r#"{{"attempt_id":{},"attempt_num":{},"started_at":{},"status":"{}""#,
            a.attempt_id,
            a.attempt_num,
            a.started_at,
            json_escape(&a.status),
        );
        if let Some(x) = a.finished_at {
            let _ = write!(s, r#","finished_at":{x}"#);
        }
        if let Some(ref x) = a.flow_id {
            let _ = write!(s, r#","flow_id":"{}""#, json_escape(x));
        }
        if let Some(ref x) = a.flow_log_path {
            let _ = write!(s, r#","flow_log_path":"{}""#, json_escape(x));
        }
        if let Some(ref x) = a.trace_id {
            let _ = write!(s, r#","trace_id":"{}""#, json_escape(x));
        }
        if let Some(x) = a.error_kind {
            let _ = write!(s, r#","error_kind":{x}"#);
        }
        if let Some(ref x) = a.error_cause {
            let _ = write!(s, r#","error_cause":"{}""#, json_escape(x));
        }
        if let Some(ref x) = a.failure_class {
            let _ = write!(s, r#","failure_class":"{}""#, json_escape(x));
        }
        s.push('}');
    }
    s.push_str("]}");
    s
}

/// `task.compact_events` — dry-run candidate counter for the
/// chronicle-retention max-age policy.
///
/// Args (pipe-delimited): `max_age_secs|mode`.
///
/// - `max_age_secs` (required, integer > 0): events older than
///   `now - max_age_secs` are candidates.
/// - `mode` (optional, default `dry-run`): only `dry-run` is
///   accepted today. Any other value returns INVALID_ARGS with
///   a clear "not implemented" cause — the destructive Step 3
///   path has not shipped, and this guard makes the boundary
///   explicit.
///
/// Returns one JSON object describing what *would* be deleted
/// under the policy. R5 honoured: only events whose parent task
/// is in a terminal state are counted, mirroring what any
/// future Step 3 deletion is constrained to touch.
fn handle_compact_events(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.compact_events utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(2, '|').collect();
    let max_age_str = parts.first().copied().unwrap_or("").trim();
    let mode = parts.get(1).copied().unwrap_or("").trim();
    if max_age_str.is_empty() {
        return invalid("task.compact_events: max_age_secs required".to_string());
    }
    let max_age_secs: i64 = match max_age_str.parse() {
        Ok(v) if v > 0 => v,
        _ => {
            return invalid(format!(
                "task.compact_events: bad max_age_secs (must be positive integer): {max_age_str}"
            ));
        }
    };
    // Mode guard: `dry-run` is the only currently-shipped mode.
    // Step 3 will add `delete` once operator-export + the
    // bounded-delete loop are both reviewed. Until then any
    // other value is INVALID_ARGS, not 500 — operators get a
    // clear "not yet" rather than a silent no-op.
    let mode = if mode.is_empty() { "dry-run" } else { mode };
    if mode != "dry-run" {
        return invalid(format!(
            "task.compact_events: mode {mode:?} not implemented; only \"dry-run\" is shipped \
             (see docs/chronicle-retention.md Step 3)"
        ));
    }
    let now = unix_secs();
    let cutoff_ts = now - max_age_secs;
    match store.count_compact_candidates(cutoff_ts) {
        Ok(result) => HandlerOutcome::Ok(render_compact_dry_run(&result, mode).into_bytes()),
        Err(e) => internal(format!("task.compact_events: {e}")),
    }
}

/// Hand-built JSON for a `CompactDryRun` result. Same approach
/// as the other Coordinator renderers — no serde_json on the
/// runtime crate.
fn render_compact_dry_run(r: &CompactDryRun, mode: &str) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(256);
    let _ = write!(
        s,
        r#"{{"mode":"{}","destructive":false,"cutoff_ts":{},"candidate_events":{},"candidate_tasks":{}"#,
        json_escape(mode),
        r.cutoff_ts,
        r.candidate_events,
        r.candidate_tasks,
    );
    if let Some(t) = r.oldest_candidate_ts {
        let _ = write!(s, r#","oldest_candidate_ts":{t}"#);
    }
    if let Some(t) = r.newest_candidate_ts {
        let _ = write!(s, r#","newest_candidate_ts":{t}"#);
    }
    s.push_str(r#","by_task_status":{"#);
    for (i, (status, n)) in r.by_task_status.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, r#""{}":{}"#, json_escape(status), n);
    }
    s.push_str("}}");
    s
}

/// `task.list_cursor` — cursor-paginated task list. Stable
/// against concurrent inserts/updates (unlike offset pagination,
/// which can repeat or skip rows when ordering ties shift). The
/// returned tail line `next_cursor=<value>` is opaque to the
/// caller — pass it back verbatim on the next request.
///
/// Wire format: `limit|status|cursor`. All three optional. Cursor
/// is `<updated_at>:<task_id>`; empty cursor returns the first
/// page. Malformed cursor is treated as empty so dashboards
/// recover from corrupted state without a 4xx.
///
/// Response: one tab-delimited row per task
/// (`task_id\tstatus\ttitle\tupdated_at`), followed by
/// `next_cursor=<value>\n` (empty value when the page was empty).
fn handle_list_cursor(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.list_cursor utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(3, '|').collect();
    let limit: usize = parts
        .first()
        .copied()
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let status_filter = parts.get(1).copied().filter(|v| !v.is_empty());
    let cursor = parts
        .get(2)
        .copied()
        .filter(|v| !v.is_empty())
        .and_then(TaskCursor::parse);
    match store.list_cursor(cursor, limit, status_filter) {
        Ok(page) => {
            let mut buf = String::new();
            for r in &page.items {
                buf.push_str(&r.task_id);
                buf.push('\t');
                buf.push_str(&r.status);
                buf.push('\t');
                let title = r.title.replace(['\t', '\n'], " ");
                buf.push_str(&title);
                buf.push('\t');
                buf.push_str(&r.updated_at.to_string());
                buf.push('\t');
                // M63: 5th column = investigation_marked_at,
                // empty when not marked. Older bridge parsers
                // splitn(4, '\t') and ignore extra fields, so
                // this is forward-compatible.
                if let Some(ts) = r.investigation_marked_at {
                    buf.push_str(&ts.to_string());
                }
                buf.push('\n');
            }
            let next = page
                .next_cursor
                .as_ref()
                .map(TaskCursor::encode)
                .unwrap_or_default();
            buf.push_str(&format!("next_cursor={next}\n"));
            HandlerOutcome::Ok(buf.into_bytes())
        }
        Err(e) => internal(format!("task.list_cursor: {e}")),
    }
}

/// `task.count` — total task count, optionally filtered by status.
/// Arg: empty or `<status>`. Returns a single line `count=N\n`.
///
/// Drives "N of M" pagination hints in operator UIs without forcing
/// them to walk every page.
fn handle_count(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.count utf8: {e}")),
    };
    let status_filter = if s.is_empty() { None } else { Some(s) };
    match store.count(status_filter) {
        Ok(n) => HandlerOutcome::Ok(format!("count={n}\n").into_bytes()),
        Err(e) => internal(format!("task.count: {e}")),
    }
}

/// `task.retry` — operator-initiated retry request. Args: `task_id`.
/// Validates the task is in failed/interrupted, the retry policy
/// permits another attempt, and the budget isn't exhausted. On
/// success transitions status to `retrying`, bumps retry_count, and
/// emits `task.retry_requested`. On exhaustion appends
/// `task.retry_exhausted`. On other rejection, returns INVALID_ARGS
/// with the cause.
///
/// Does NOT re-run the flow. Re-execution is owned by whoever runs
/// the flow (bridge auto-retry is not wired today). Returns one
/// line: `accepted attempt=N of budget`, `exhausted retry_count=N
/// budget=M`, or just the rejection cause.
fn handle_retry(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.retry utf8: {e}")),
    };
    if task_id.is_empty() {
        return invalid("task.retry: task_id required".to_string());
    }
    match store.request_retry(task_id) {
        Ok(RetryDecision::Accepted {
            new_retry_count,
            budget,
        }) => {
            let body = format!("accepted attempt={new_retry_count} of_budget={budget}\n");
            HandlerOutcome::Ok(body.into_bytes())
        }
        Ok(RetryDecision::Exhausted {
            retry_count,
            budget,
        }) => {
            // Exhaustion is a normal, expected outcome — surface as
            // OK with structured body, not as an error envelope.
            // Operators decide what to do next (cancel, raise budget,
            // investigate).
            let body = format!("exhausted retry_count={retry_count} budget={budget}\n");
            HandlerOutcome::Ok(body.into_bytes())
        }
        Ok(RetryDecision::Rejected { reason }) => invalid(format!("task.retry: {reason}")),
        Err(CoordinatorError::NotFound(id)) => invalid(format!("task.retry: not found: {id}")),
        Err(e) => internal(format!("task.retry: {e}")),
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

/// `task.edges` — list every execution edge that touches
/// the given task (as child or parent). One tab-delimited
/// row per edge:
///
///   edge_id \t edge_type \t attempt_id|- \t
///     related_task_id|- \t related_attempt_id|- \t
///     spawned_by_event_id|- \t created_at
///
/// Phase-1E M38: only `retried_from` is emitted today.
/// Other edge types in the schema (spawned, blocked_on,
/// resumed_from, delegated_to, parallel_branch, awaited)
/// have no shipped emitters yet — they're reserved
/// vocabulary so future runtime primitives don't need a
/// schema bump.
fn handle_edges(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.edges utf8: {e}")),
    };
    if task_id.is_empty() {
        return invalid("task.edges: task_id required".to_string());
    }
    match store.list_edges_for_task(task_id) {
        Ok(rows) => {
            let mut buf = String::new();
            for e in rows {
                let aid = e
                    .attempt_id
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "-".into());
                let rt = e.related_task_id.as_deref().unwrap_or("-");
                let rat = e
                    .related_attempt_id
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "-".into());
                let sev = e
                    .spawned_by_event_id
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "-".into());
                buf.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    e.edge_id, e.edge_type, aid, rt, rat, sev, e.created_at,
                ));
            }
            HandlerOutcome::Ok(buf.into_bytes())
        }
        Err(e) => internal(format!("task.edges: {e}")),
    }
}

/// `task.recent_edges` — cross-task aggregate of the most
/// recent execution edges. Args: `since_edge_id|limit`
/// (both optional; defaults 0 / 50). Returns one
/// tab-delimited row per edge, newest-first, same column
/// layout as `task.edges`. Operators use this to spot
/// retry-storm patterns without per-task drill-in.
fn handle_recent_edges(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.recent_edges utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(2, '|').collect();
    let since: i64 = parts
        .first()
        .copied()
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().unwrap_or(0))
        .unwrap_or(0);
    let limit: usize = parts
        .get(1)
        .copied()
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().unwrap_or(50))
        .unwrap_or(50);
    match store.list_recent_edges(since, limit) {
        Ok(rows) => {
            let mut buf = String::new();
            for e in rows {
                let aid = e
                    .attempt_id
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "-".into());
                let rt = e.related_task_id.as_deref().unwrap_or("-");
                let rat = e
                    .related_attempt_id
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "-".into());
                let sev = e
                    .spawned_by_event_id
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "-".into());
                buf.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    e.edge_id, e.edge_type, e.task_id, aid, rt, rat, sev, e.created_at,
                ));
            }
            HandlerOutcome::Ok(buf.into_bytes())
        }
        Err(e) => internal(format!("task.recent_edges: {e}")),
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

/// Hard cap on a single operator note's length. Picked to
/// fit a paragraph of triage context (a few sentences) without
/// inviting people to dump multi-KB logs into the chronicle —
/// that's what `task.export` + on-disk grep are for.
pub const MAX_OPERATOR_NOTE_LEN: usize = 2_000;

/// `task.note` — operator-authored chronicle annotation. Args:
/// `task_id|note_text`. The note text may contain `|` (splitn(2)
/// keeps the remainder intact). The author is taken from
/// `ctx.caller.subject_id` so the recorded note matches the
/// admission-audit caller for the same RPC.
///
/// Returns `event_id=N\n` on success so callers can deep-link
/// into the chronicle. INVALID_ARGS when:
/// - args malformed (missing `|` separator)
/// - task_id empty
/// - note text empty after trim
/// - note text exceeds [`MAX_OPERATOR_NOTE_LEN`]
fn handle_note(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.note utf8: {e}")),
    };
    let mut parts = s.splitn(2, '|');
    let task_id = parts.next().unwrap_or("");
    let note = parts.next().unwrap_or("");
    if task_id.is_empty() {
        return invalid("task.note: task_id required (arg shape: task_id|note)".to_string());
    }
    if note.is_empty() {
        return invalid("task.note: note text required (arg shape: task_id|note)".to_string());
    }
    let author = ctx.caller.subject_id.to_string();
    match store.append_operator_note(task_id, note, &author) {
        Ok(event_id) => HandlerOutcome::Ok(format!("event_id={event_id}\n").into_bytes()),
        Err(CoordinatorError::NotFound(id)) => invalid(format!("task.note: not found: {id}")),
        Err(CoordinatorError::Invalid(msg)) => invalid(msg),
        Err(e) => internal(format!("task.note: {e}")),
    }
}

/// `task.mark_investigation` — operator-set per-task
/// investigation flag (M62). Args: `task_id|0|...` to clear,
/// `task_id|1|<reason>` to mark. Reason is optional even on
/// mark (operators flagging quickly don't always have one);
/// splitn(3) keeps `|` in the reason intact.
///
/// Returns `marked_at=<ts>` on a mark, `marked_at=` (empty)
/// after a clear, so callers can parse a single shape.
fn handle_mark_investigation(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.mark_investigation utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(3, '|').collect();
    let task_id = parts.first().copied().unwrap_or("");
    let marked_flag = parts.get(1).copied().unwrap_or("");
    let reason = parts.get(2).copied().filter(|v| !v.is_empty());
    if task_id.is_empty() {
        return invalid(
            "task.mark_investigation: task_id required (arg shape: task_id|0|1|reason)".to_string(),
        );
    }
    let marked = match marked_flag {
        "1" | "true" => true,
        "0" | "false" | "" => false,
        other => {
            return invalid(format!(
                "task.mark_investigation: invalid marked flag '{other}' (expected 0|1)"
            ));
        }
    };
    let author = ctx.caller.subject_id.to_string();
    match store.set_investigation_marker(task_id, marked, reason, &author) {
        Ok(Some(ts)) => HandlerOutcome::Ok(format!("marked_at={ts}\n").into_bytes()),
        Ok(None) => HandlerOutcome::Ok(b"marked_at=\n".to_vec()),
        Err(CoordinatorError::NotFound(id)) => {
            invalid(format!("task.mark_investigation: not found: {id}"))
        }
        Err(CoordinatorError::Invalid(msg)) => invalid(msg),
        Err(e) => internal(format!("task.mark_investigation: {e}")),
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
    if let Some(x) = v.investigation_marked_at {
        let _ = writeln!(s, "investigation_marked_at={}", x);
    }
    if let Some(x) = v.investigation_reason.as_ref() {
        let _ = writeln!(s, "investigation_reason={}", x);
    }
    let _ = writeln!(s, "event_count={}", v.events.len());
    // Events as a simple JSON array. We hand-build the JSON to avoid
    // pulling serde_json into this hot path; payloads are escaped
    // minimally. S2-structured events also surface schema_version,
    // attempt_id, trace_id, payload_json — older renderers that
    // only look at id/ts/type/payload keep working since those four
    // keys come first.
    s.push_str("events=[");
    for (i, ev) in v.events.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&render_event_json(ev));
    }
    s.push_str("]\n");
    s
}

/// Hand-built JSON for one event. Used by both the chronicle
/// inline array (`render_task_view`) and the streaming
/// `task.events` body. Field order is stable: `id`, `ts`, `type`,
/// `payload` come first so legacy parsers see them at predictable
/// positions; the typed envelope keys land after, omitted entirely
/// when null/0 so v0 events stay byte-identical to the pre-S2
/// shape.
fn render_event_json(ev: &TaskEvent) -> String {
    let mut s = String::with_capacity(128);
    s.push_str(&format!(
        r#"{{"id":{},"ts":{},"type":"{}","payload":"{}""#,
        ev.event_id,
        ev.ts,
        json_escape(&ev.event_type),
        json_escape(&ev.payload),
    ));
    if ev.schema_version != 0 {
        s.push_str(&format!(r#","schema_version":{}"#, ev.schema_version));
    }
    if let Some(aid) = ev.attempt_id {
        s.push_str(&format!(r#","attempt_id":{aid}"#));
    }
    if let Some(t) = ev.trace_id.as_deref() {
        s.push_str(&format!(r#","trace_id":"{}""#, json_escape(t)));
    }
    if let Some(pj) = ev.payload_json.as_deref() {
        // payload_json is already valid JSON (object/array/value).
        // Embed verbatim so consumers can parse without a double-
        // decode step.
        s.push_str(r#","payload_json":"#);
        s.push_str(pj);
    }
    s.push('}');
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

        -- Phase-1E M38: explicit execution edges between attempts
        -- and (eventually) between tasks. Every edge must
        -- originate from a recorded runtime action — no
        -- synthesized causality. The first emitter is the
        -- retry path (request_retry), which links a new
        -- attempt back to the failed one as `retried_from`.
        --
        -- edge_type vocabulary (reserved; only the first
        -- ships with an emitter today):
        --   retried_from      — new attempt N follows prior
        --                       attempt M after task.retry.
        --                       SHIPPED.
        --   spawned           — child task spawned by a
        --                       parent. Reserved; no
        --                       task-spawning primitive yet.
        --   blocked_on        — task blocked awaiting a
        --                       dependency. Reserved.
        --   resumed_from      — durable yield resume. Reserved
        --                       (Gate 2 resumable VM).
        --   delegated_to      — sub-flow delegation. Reserved.
        --   parallel_branch   — concurrent SOL branch.
        --                       Reserved.
        --   awaited           — async wait primitive. Reserved.
        --
        -- The shape supports cross-task edges (related_task_id)
        -- and per-attempt edges (related_attempt_id) so future
        -- emitters don't need a second schema bump.
        CREATE TABLE IF NOT EXISTS task_edges (
            edge_id             INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id             TEXT    NOT NULL,
            attempt_id          INTEGER,
            edge_type           TEXT    NOT NULL,
            related_task_id     TEXT,
            related_attempt_id  INTEGER,
            spawned_by_event_id INTEGER,
            created_at          INTEGER NOT NULL,
            FOREIGN KEY (task_id) REFERENCES tasks(task_id)
        );
        CREATE INDEX IF NOT EXISTS task_edges_by_task
            ON task_edges(task_id, edge_id);
        CREATE INDEX IF NOT EXISTS task_edges_by_related
            ON task_edges(related_task_id);
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
        // S2: typed event envelopes. schema_version=0 = legacy
        // (string payload only). schema_version=1 = structured;
        // attempt_id / trace_id / payload_json may be set.
        "ALTER TABLE task_events ADD COLUMN schema_version INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE task_events ADD COLUMN attempt_id INTEGER",
        "ALTER TABLE task_events ADD COLUMN trace_id TEXT",
        "ALTER TABLE task_events ADD COLUMN payload_json TEXT",
        // M62: operator-set investigation marker. NULL when not
        // marked; set to unix_secs() of the most recent
        // `task.mark_investigation` call. Clearing the marker
        // writes NULL (and emits a `task.investigation_cleared`
        // chronicle event). The marker is per-task durable
        // state — operators flag tasks they want to come back
        // to without polluting the task list with manual notes.
        "ALTER TABLE tasks ADD COLUMN investigation_marked_at INTEGER",
        // Optional short operator-supplied reason captured at
        // mark time; surfaced in the dashboard banner so the
        // marker isn't just a flag in isolation.
        "ALTER TABLE tasks ADD COLUMN investigation_reason TEXT",
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

// ──────────────────────────── Typed event emit (S2) ────────────────────────

/// Insert a structured event envelope (schema_version=1) inside
/// an open transaction. The legacy `payload` string is also
/// populated for back-compat with renderers that haven't been
/// upgraded; the typed JSON in `payload_json` is the
/// authoritative form going forward.
///
/// Runtime-only helper — operator-defined events via the
/// `task.event` capability remain v0 (string-only) by design,
/// since per-event-type schemas are runtime concerns.
#[allow(clippy::too_many_arguments)]
fn insert_typed_event(
    tx: &rusqlite::Transaction<'_>,
    task_id: &str,
    ts: i64,
    event_type: &str,
    legacy_payload: &str,
    attempt_id: Option<i64>,
    trace_id: Option<&str>,
    payload_json: Option<&str>,
) -> Result<(), CoordinatorError> {
    tx.execute(
        "INSERT INTO task_events
            (task_id, ts, event_type, payload,
             schema_version, attempt_id, trace_id, payload_json)
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7)",
        params![
            task_id,
            ts,
            event_type,
            legacy_payload,
            attempt_id,
            trace_id,
            payload_json,
        ],
    )
    .map_err(CoordinatorError::Db)?;
    Ok(())
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
    // Capture the prior attempt's id BEFORE opening the new one
    // — we'll need it for the retried_from edge when count > 0.
    let prior_attempt_id = current_id;
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
    let legacy = match trace_id {
        Some(t) => format!("attempt_id={new_id} attempt_num={next_num} trace_id={t}"),
        None => format!("attempt_id={new_id} attempt_num={next_num}"),
    };
    let payload_json = match trace_id {
        Some(t) => format!(
            r#"{{"attempt_id":{new_id},"attempt_num":{next_num},"trace_id":"{}"}}"#,
            json_escape(t)
        ),
        None => format!(r#"{{"attempt_id":{new_id},"attempt_num":{next_num}}}"#),
    };
    insert_typed_event(
        tx,
        task_id,
        now,
        "task.attempt_started",
        &legacy,
        Some(new_id),
        trace_id,
        Some(&payload_json),
    )?;

    // Phase-1E M38: record the retried_from execution edge
    // when the new attempt follows a prior closed attempt.
    // The edge points the new attempt back at the prior one
    // and (when discoverable) at the chronicle event that
    // triggered the retry (`task.retry_requested`).
    //
    // Only retried_from is emitted today — other edge types
    // in the task_edges schema (`spawned`, `blocked_on`,
    // `delegated_to`, `parallel_branch`, `awaited`,
    // `resumed_from`) need runtime primitives Relix doesn't
    // ship yet.
    if next_num > 1
        && let Some(prior_id) = prior_attempt_id
    {
        // Find the most recent task.retry_requested event for
        // this task — it's the chronicle anchor for the retry.
        // Falls back to None when missing (e.g. tasks that
        // retried before this instrumentation landed).
        let trigger_event: Option<i64> = tx
            .query_row(
                "SELECT event_id FROM task_events
                 WHERE task_id = ?1 AND event_type = 'task.retry_requested'
                 ORDER BY event_id DESC LIMIT 1",
                params![task_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(CoordinatorError::Db)?;
        tx.execute(
            "INSERT INTO task_edges
                (task_id, attempt_id, edge_type, related_task_id,
                 related_attempt_id, spawned_by_event_id, created_at)
             VALUES (?1, ?2, 'retried_from', ?1, ?3, ?4, ?5)",
            params![task_id, new_id, prior_id, trigger_event, now],
        )
        .map_err(CoordinatorError::Db)?;
    }

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
    let mut legacy = format!("attempt_id={aid} status={new_status}");
    if let Some(fc) = failure_class {
        legacy.push_str(&format!(" failure_class={fc}"));
    }
    let payload_json = match failure_class {
        Some(fc) => format!(
            r#"{{"attempt_id":{aid},"status":"{}","failure_class":"{}"}}"#,
            json_escape(new_status),
            json_escape(fc),
        ),
        None => format!(
            r#"{{"attempt_id":{aid},"status":"{}"}}"#,
            json_escape(new_status)
        ),
    };
    insert_typed_event(
        tx,
        task_id,
        now,
        "task.attempt_finished",
        &legacy,
        Some(aid),
        None,
        Some(&payload_json),
    )?;
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
    /// Caller-supplied input failed a TaskStore-level validation
    /// (empty required field, oversize input, etc.). Mapped to
    /// `INVALID_ARGS` at the handler boundary so the caller
    /// sees a user-friendly message rather than a generic
    /// internal error.
    #[error("invalid: {0}")]
    Invalid(String),
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

    // ── C2c: operator-initiated retry primitive ──────────────────────

    #[test]
    fn request_retry_rejects_on_pending_status() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        match s.request_retry(&tid).unwrap() {
            RetryDecision::Rejected { reason } => assert!(reason.contains("pending")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn request_retry_rejects_on_retry_policy_none() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        // Drive into failed state.
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            Some("oops"),
            Some("transient"),
        )
        .unwrap();
        match s.request_retry(&tid).unwrap() {
            RetryDecision::Rejected { reason } => assert!(reason.contains("retry_policy=none")),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[test]
    fn request_retry_accepted_then_exhausted_with_once_policy() {
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::Once, 0, None)
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
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
        // First retry: accepted.
        match s.request_retry(&tid).unwrap() {
            RetryDecision::Accepted {
                new_retry_count,
                budget,
            } => {
                assert_eq!(new_retry_count, 1);
                assert_eq!(budget, 1);
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.status, "retrying");
        assert_eq!(v.retry_count, 1);
        assert!(
            v.events
                .iter()
                .any(|e| e.event_type == "task.retry_requested")
        );
        // Second retry without re-failing: still in retrying (not
        // failed/interrupted) so rejected.
        match s.request_retry(&tid).unwrap() {
            RetryDecision::Rejected { reason } => assert!(reason.contains("retrying")),
            other => panic!("expected Rejected, got {other:?}"),
        }
        // Fail again, then re-request: now exhausted because
        // retry_count (1) >= budget (1).
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            Some("blip2"),
            Some("transient"),
        )
        .unwrap();
        match s.request_retry(&tid).unwrap() {
            RetryDecision::Exhausted {
                retry_count,
                budget,
            } => {
                assert_eq!(retry_count, 1);
                assert_eq!(budget, 1);
            }
            other => panic!("expected Exhausted, got {other:?}"),
        }
        let v = s.get(&tid).unwrap().unwrap();
        assert!(
            v.events
                .iter()
                .any(|e| e.event_type == "task.retry_exhausted")
        );
        // Still in failed (exhausted does not flip status).
        assert_eq!(v.status, "failed");
    }

    #[test]
    fn request_retry_clears_error_columns_but_preserves_last_failure_record() {
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, None)
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            Some(error_kinds::TRANSPORT as i64),
            Some("the actual cause"),
            Some("transient"),
        )
        .unwrap();
        s.request_retry(&tid).unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        // Transient error_* columns cleared on the task row...
        assert_eq!(v.error_kind, None);
        assert_eq!(v.error_cause, None);
        // ...but the persistent failure record is preserved for
        // operator triage.
        assert_eq!(v.last_failure_class.as_deref(), Some("transient"));
        assert_eq!(v.last_failure_reason.as_deref(), Some("the actual cause"));
    }

    #[test]
    fn request_retry_unknown_task_is_invalid() {
        let s = store();
        match s.request_retry("nope") {
            Err(CoordinatorError::NotFound(id)) => assert_eq!(id, "nope"),
            other => panic!("expected NotFound, got {other:?}"),
        }
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

    // ── Track 6 hardening: coordinator edge cases ─────────────────────

    #[test]
    fn large_chronicle_renders_without_truncation() {
        // 500 events should render cleanly through render_task_view
        // (regression guard for any future cap that might silently
        // drop events).
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        for i in 0..500 {
            s.append_event(&tid, "checkpoint", &format!("step={i}"))
                .unwrap();
        }
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.events.len(), 500);
        let rendered = render_task_view(&v);
        assert!(rendered.contains("event_count=500"));
        // First and last events both present in JSON array.
        assert!(rendered.contains("step=0"));
        assert!(rendered.contains("step=499"));
    }

    #[test]
    fn event_payload_with_special_chars_round_trips_safely() {
        // Payload with quotes, backslashes, newlines, tabs, and a
        // low control byte. Must survive store + get + render
        // without breaking the JSON escape contract.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let payload = "quote=\" backslash=\\ newline=\n tab=\t ctrl=\x01 end";
        s.append_event(&tid, "checkpoint", payload).unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.events[0].payload, payload);
        let rendered = render_task_view(&v);
        // JSON-escaped forms appear in the rendered events array.
        assert!(rendered.contains("\\\""));
        assert!(rendered.contains("\\\\"));
        assert!(rendered.contains("\\n"));
        assert!(rendered.contains("\\t"));
        // Control byte rendered as \uXXXX escape, not raw.
        assert!(rendered.contains("\\u0001"));
    }

    #[test]
    fn concurrent_create_and_recover_dont_corrupt_attempt_count() {
        // Mini stress test: run 10 create+update cycles serially and
        // confirm attempt_count tracks correctly. (Real concurrency
        // testing requires multiple stores against the same DB; this
        // covers the single-store serialized path.)
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 99, Some(60))
            .unwrap();
        for _ in 0..10 {
            s.update(&tid, Some("running"), None, None, None, None, None, None)
                .unwrap();
            s.update(
                &tid,
                Some("failed"),
                None,
                None,
                None,
                None,
                Some("flake"),
                Some("transient"),
            )
            .unwrap();
            s.request_retry(&tid).unwrap();
        }
        let v = s.get(&tid).unwrap().unwrap();
        // 10 attempts opened (each running transition after retry
        // opens a new one).
        assert_eq!(v.attempt_count, 10);
        let attempts = s.list_attempts(&tid).unwrap();
        assert_eq!(attempts.len(), 10);
        // Each closed as failed.
        assert!(attempts.iter().all(|a| a.status == "failed"));
        // retry_count incremented per request_retry call (10 - 1
        // since the first running used the initial slot and request_retry
        // bumps it for the SECOND through 10th).
        assert!(v.retry_count >= 1, "retry_count={}", v.retry_count);
    }

    // ── Priority A: pagination + count ────────────────────────────────

    #[test]
    fn list_paginated_offset_skips_rows() {
        let s = store();
        // Create 5 tasks with deterministic-ish ordering: each new
        // task is the most-recently-updated, so list returns them
        // in reverse-creation order.
        for i in 0..5 {
            mk(&s, &format!("t{i}"), "f", "{}", "o");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let page1 = s.list_paginated(2, 0, None).unwrap();
        let page2 = s.list_paginated(2, 2, None).unwrap();
        let page3 = s.list_paginated(2, 4, None).unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page2.len(), 2);
        assert_eq!(page3.len(), 1);
        // No duplicates across pages.
        let ids: std::collections::HashSet<String> = page1
            .iter()
            .chain(page2.iter())
            .chain(page3.iter())
            .map(|r| r.task_id.clone())
            .collect();
        assert_eq!(ids.len(), 5);
    }

    #[test]
    fn list_paginated_status_filter_only_matches() {
        let s = store();
        let t1 = mk(&s, "a", "f", "{}", "o");
        let _t2 = mk(&s, "b", "f", "{}", "o");
        let t3 = mk(&s, "c", "f", "{}", "o");
        s.update(&t1, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(&t3, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let running = s.list_paginated(50, 0, Some("running")).unwrap();
        assert_eq!(running.len(), 2);
        assert!(running.iter().all(|r| r.status == "running"));
        let pending = s.list_paginated(50, 0, Some("pending")).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, "pending");
        let none_match = s.list_paginated(50, 0, Some("nope")).unwrap();
        assert!(none_match.is_empty());
    }

    #[test]
    fn count_total_and_filtered() {
        let s = store();
        for i in 0..7 {
            let tid = mk(&s, &format!("t{i}"), "f", "{}", "o");
            if i % 2 == 0 {
                s.update(&tid, Some("running"), None, None, None, None, None, None)
                    .unwrap();
            }
        }
        assert_eq!(s.count(None).unwrap(), 7);
        assert_eq!(s.count(Some("pending")).unwrap(), 3);
        assert_eq!(s.count(Some("running")).unwrap(), 4);
        assert_eq!(s.count(Some("nope")).unwrap(), 0);
    }

    #[test]
    fn list_events_after_incremental_fetch() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let e1 = s.append_event(&tid, "step", "a").unwrap();
        let e2 = s.append_event(&tid, "step", "b").unwrap();
        let e3 = s.append_event(&tid, "step", "c").unwrap();
        // From the beginning.
        let all = s.list_events_after(&tid, 0, 100).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].event_id, e1);
        // After e1.
        let after1 = s.list_events_after(&tid, e1, 100).unwrap();
        assert_eq!(after1.len(), 2);
        assert_eq!(after1[0].event_id, e2);
        // After the latest — empty.
        let after3 = s.list_events_after(&tid, e3, 100).unwrap();
        assert!(after3.is_empty());
    }

    #[test]
    fn list_events_after_respects_limit_cap() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        for i in 0..10 {
            s.append_event(&tid, "step", &format!("e{i}")).unwrap();
        }
        let chunk = s.list_events_after(&tid, 0, 3).unwrap();
        assert_eq!(chunk.len(), 3);
        let next = s.list_events_after(&tid, chunk[2].event_id, 3).unwrap();
        assert_eq!(next.len(), 3);
        // No overlap.
        assert!(next[0].event_id > chunk[2].event_id);
    }

    #[test]
    fn list_events_after_unknown_task_is_not_found() {
        let s = store();
        match s.list_events_after("deadbeef", 0, 10) {
            Err(CoordinatorError::NotFound(_)) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    // ── Phase H: operator export (S5 / retention groundwork) ──────────

    #[test]
    fn export_task_full_round_trip() {
        use serde_json::Value;
        let s = store();
        let tid = s
            .create(
                "export test",
                "f.sol",
                "{}",
                "alice",
                RetryPolicy::Bounded,
                3,
                Some(60),
            )
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.append_event(&tid, "ops.custom", "operator note").unwrap();
        s.update(
            &tid,
            Some("completed"),
            Some("done"),
            Some("flow123"),
            Some("/tmp/x.log"),
            None,
            None,
            None,
        )
        .unwrap();

        let export = s.export_task(&tid).unwrap();
        assert_eq!(export.task_id, tid);
        assert_eq!(export.view.status, "completed");
        assert_eq!(export.attempts.len(), 1);
        // Chronicle includes both runtime-emitted events and
        // the operator-defined one.
        assert!(
            export
                .view
                .events
                .iter()
                .any(|e| e.event_type == "ops.custom")
        );
        assert!(
            export
                .view
                .events
                .iter()
                .any(|e| e.event_type == "task.attempt_started")
        );

        // Render path produces parseable JSON.
        let body = render_task_export(&export);
        let v: Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("export body did not parse:\n{body}\nerror: {e}"));
        assert_eq!(v.get("schema_version").and_then(|x| x.as_u64()), Some(1));
        assert!(v.get("exported_at").and_then(|x| x.as_i64()).unwrap_or(0) > 0);
        assert_eq!(
            v.get("task_id").and_then(|x| x.as_str()),
            Some(tid.as_str())
        );
        let task = v.get("task").expect("task field");
        assert_eq!(
            task.get("status").and_then(|x| x.as_str()),
            Some("completed")
        );
        // Events array must be present and non-empty.
        let events = task
            .get("events")
            .and_then(|x| x.as_array())
            .expect("events");
        assert!(!events.is_empty());
        // Attempts array.
        let attempts = v
            .get("attempts")
            .and_then(|x| x.as_array())
            .expect("attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(
            attempts[0].get("status").and_then(|x| x.as_str()),
            Some("completed")
        );
    }

    #[test]
    fn export_task_unknown_id_is_not_found() {
        let s = store();
        match s.export_task("deadbeef") {
            Err(CoordinatorError::NotFound(id)) => assert_eq!(id, "deadbeef"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn export_task_renders_with_hostile_chars_safely() {
        // Operator-defined event with chars that would break
        // naive JSON escaping. The export body must round-trip.
        let s = store();
        let tid = mk(&s, "t\"with quotes\"", "f", "{}", "o");
        s.append_event(&tid, "ops.test", "key=\"q\" backslash=\\ ctrl=\x01 tab=\t")
            .unwrap();
        let export = s.export_task(&tid).unwrap();
        let body = render_task_export(&export);
        let _v: serde_json::Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("hostile export failed to parse:\n{body}\nerror: {e}"));
    }

    // ── Phase I: hardening for typed envelopes + cursors ──────────────

    #[test]
    fn every_runtime_emitted_payload_json_is_valid_json() {
        // Regression guard: a future emitter that mishandles
        // escapes would silently produce broken JSON. Exercise
        // every v1 emit path and assert each payload_json parses.
        use serde_json::Value;
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, Some(5))
            .unwrap();
        // attempt_started + attempt_finished
        s.update_with_trace(
            &tid,
            Some("running"),
            None,
            None,
            None,
            None,
            None,
            None,
            Some("00112233445566778899aabbccddeeff"),
        )
        .unwrap();
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            Some("error \"with quotes\" and \\backslash"),
            Some("transient"),
        )
        .unwrap();
        // retry_requested
        s.request_retry(&tid).unwrap();
        // Drive into interrupted via recovery scan.
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let started = s.list_attempts(&tid).unwrap().last().unwrap().started_at;
        s.recover_interrupted(started + 60).unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        let mut json_count = 0;
        for ev in &v.events {
            if let Some(pj) = ev.payload_json.as_deref() {
                let _v: Value = serde_json::from_str(pj).unwrap_or_else(|e| {
                    panic!(
                        "event '{}' has invalid payload_json: {pj}\nerror: {e}",
                        ev.event_type
                    )
                });
                json_count += 1;
            }
        }
        assert!(
            json_count >= 4,
            "expected several v1 events with payload_json, got {json_count}"
        );
    }

    #[test]
    fn render_event_json_emits_parseable_lines_under_hostile_payloads() {
        // The line-delimited body that task.events emits must
        // round-trip through serde_json one line at a time. A
        // malformed escape in any single event would break
        // every downstream parser. Exercise an operator-defined
        // v0 event with chars that would break naive escaping.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.append_event(&tid, "ops.test", "key=\"q\" backslash=\\ ctrl=\x01 tab=\t")
            .unwrap();
        let events = s.list_events_after(&tid, 0, 10).unwrap();
        assert_eq!(events.len(), 1);
        let line = render_event_json(&events[0]);
        let parsed: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("rendered event line did not parse:\n{line}\nerror: {e}"));
        assert_eq!(
            parsed.get("type").and_then(|v| v.as_str()),
            Some("ops.test")
        );
    }

    #[test]
    fn list_cursor_with_updated_target_pushed_above_does_not_repeat() {
        // If a task has been touched (updated_at bumped) between
        // page 1 and page 2, the row may now sit above its own
        // cursor. The cursor's strict-less-than WHERE clause
        // filters it; it does not reappear on page 2 — the
        // load-bearing snapshot contract.
        let s = store();
        let mut ids = Vec::new();
        for i in 0..5 {
            ids.push(mk(&s, &format!("t{i}"), "f", "{}", "o"));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let page1 = s.list_cursor(None, 2, None).unwrap();
        assert_eq!(page1.items.len(), 2);
        let cursor = page1.next_cursor.clone();
        // Bump the cursor's own target above itself.
        let target_id = page1.items.last().unwrap().task_id.clone();
        std::thread::sleep(std::time::Duration::from_millis(10));
        s.update(
            &target_id,
            Some("running"),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let page2 = s.list_cursor(cursor, 100, None).unwrap();
        let page2_ids: std::collections::HashSet<String> =
            page2.items.iter().map(|r| r.task_id.clone()).collect();
        assert!(
            !page2_ids.contains(&target_id),
            "bumped target reappeared on page 2 — cursor snapshot broken"
        );
    }

    #[test]
    fn task_cursor_parse_resists_pathological_inputs() {
        // Operator / proxy could mangle the opaque cursor token.
        // Parser must never panic. Return None on malformed
        // shapes; the capability handler treats None as "start
        // from beginning" so polling tools recover.
        let inputs = [
            "",
            ":",
            "abc:def",
            "123:",
            "-1:taskid", // negative updated_at; parses fine
            "0:",        // empty task_id
            ":0",        // empty updated_at
            "very long input that doesn't look like a cursor",
            "0:taskid:extra", // extra colon — split_once stops at first
        ];
        for s in inputs {
            let _ = TaskCursor::parse(s); // must not panic
        }
    }

    // ── Phase S2: typed event envelopes ───────────────────────────────

    #[test]
    fn attempt_started_event_has_typed_fields() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update_with_trace(
            &tid,
            Some("running"),
            None,
            None,
            None,
            None,
            None,
            None,
            Some("00112233445566778899aabbccddeeff"),
        )
        .unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        let ev = v
            .events
            .iter()
            .find(|e| e.event_type == "task.attempt_started")
            .expect("attempt_started event missing");
        assert_eq!(ev.schema_version, 1);
        assert!(ev.attempt_id.is_some());
        assert_eq!(
            ev.trace_id.as_deref(),
            Some("00112233445566778899aabbccddeeff")
        );
        let pj = ev.payload_json.as_deref().expect("payload_json populated");
        assert!(pj.contains("\"attempt_id\""));
        assert!(pj.contains("\"trace_id\":\"00112233445566778899aabbccddeeff\""));
    }

    #[test]
    fn attempt_finished_event_has_typed_fields() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            Some("oops"),
            Some("transient"),
        )
        .unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        let ev = v
            .events
            .iter()
            .find(|e| e.event_type == "task.attempt_finished")
            .expect("attempt_finished event missing");
        assert_eq!(ev.schema_version, 1);
        assert!(ev.attempt_id.is_some());
        let pj = ev.payload_json.as_deref().expect("payload_json populated");
        assert!(pj.contains("\"status\":\"failed\""));
        assert!(pj.contains("\"failure_class\":\"transient\""));
    }

    #[test]
    fn task_interrupted_event_has_typed_fields() {
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::None, 0, Some(5))
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let started = s.list_attempts(&tid).unwrap()[0].started_at;
        s.recover_interrupted(started + 60).unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        let ev = v
            .events
            .iter()
            .find(|e| e.event_type == "task.interrupted")
            .expect("task.interrupted event missing");
        assert_eq!(ev.schema_version, 1);
        let pj = ev.payload_json.as_deref().expect("payload_json populated");
        assert!(pj.contains("\"reason\":\"deadline_exceeded\""));
        assert!(pj.contains("\"started_at\""));
    }

    #[test]
    fn retry_requested_event_has_typed_fields() {
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, None)
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
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
        s.request_retry(&tid).unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        let ev = v
            .events
            .iter()
            .find(|e| e.event_type == "task.retry_requested")
            .expect("task.retry_requested event missing");
        assert_eq!(ev.schema_version, 1);
        let pj = ev.payload_json.as_deref().expect("payload_json populated");
        assert!(pj.contains("\"attempt\":1"));
        assert!(pj.contains("\"policy\":\"bounded\""));
        assert!(pj.contains("\"prior_class\":\"transient\""));
    }

    #[test]
    fn operator_event_via_append_event_stays_v0() {
        // Operator-defined events via the `task.event` capability
        // are NOT auto-promoted to v1 — schemas per event_type are
        // a runtime concern, and we don't want to fake structure
        // we don't have.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.append_event(&tid, "ops.custom", "anything goes").unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        let ev = v
            .events
            .iter()
            .find(|e| e.event_type == "ops.custom")
            .expect("custom event missing");
        assert_eq!(ev.schema_version, 0);
        assert!(ev.attempt_id.is_none());
        assert!(ev.trace_id.is_none());
        assert!(ev.payload_json.is_none());
        assert_eq!(ev.payload, "anything goes");
    }

    // ── Phase S1: cursor pagination ───────────────────────────────────

    #[test]
    fn list_cursor_first_page_has_no_input_cursor() {
        let s = store();
        for i in 0..5 {
            mk(&s, &format!("t{i}"), "f", "{}", "o");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let page = s.list_cursor(None, 3, None).unwrap();
        assert_eq!(page.items.len(), 3);
        // next_cursor is the last row of THIS page.
        let last = page.items.last().unwrap();
        let c = page.next_cursor.expect("non-empty page must yield cursor");
        assert_eq!(c.updated_at, last.updated_at);
        assert_eq!(c.task_id, last.task_id);
    }

    #[test]
    fn list_cursor_walks_pages_without_duplicates_or_skips() {
        let s = store();
        let mut ids = Vec::new();
        for i in 0..10 {
            ids.push(mk(&s, &format!("t{i}"), "f", "{}", "o"));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let mut cursor = None;
        let mut seen = Vec::new();
        loop {
            let page = s.list_cursor(cursor.clone(), 3, None).unwrap();
            if page.items.is_empty() {
                break;
            }
            for r in &page.items {
                seen.push(r.task_id.clone());
            }
            cursor = page.next_cursor;
        }
        assert_eq!(seen.len(), 10);
        // No duplicates.
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 10);
        // All 10 created ids are present.
        for tid in &ids {
            assert!(seen.contains(tid), "task {tid} missing from cursor walk");
        }
    }

    #[test]
    fn list_cursor_stable_under_concurrent_inserts() {
        // After getting page 1, insert new rows that bump the
        // ordering. Page 2 (cursor-based) must NOT show rows that
        // appeared at the top of the ordering AFTER page 1 was
        // taken. (Offset-based pagination would have shown / hidden
        // duplicates here; the cursor pins the snapshot.)
        let s = store();
        let mut original_ids = Vec::new();
        for i in 0..6 {
            original_ids.push(mk(&s, &format!("orig{i}"), "f", "{}", "o"));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let page1 = s.list_cursor(None, 3, None).unwrap();
        assert_eq!(page1.items.len(), 3);
        // Simulate concurrent activity: new rows + an update bumping
        // an original row's updated_at to "now".
        for i in 0..3 {
            mk(&s, &format!("new{i}"), "f", "{}", "o");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Bump one of the original rows.
        s.update(
            &original_ids[0],
            Some("running"),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let page2 = s.list_cursor(page1.next_cursor.clone(), 100, None).unwrap();
        // Original tasks orig0..orig5 minus the 3 we saw on page 1
        // should appear, plus orig0 should NOT reappear (its
        // updated_at jumped above the cursor's, so the WHERE clause
        // filters it out — exactly the stable-snapshot contract).
        let page1_ids: std::collections::HashSet<String> =
            page1.items.iter().map(|r| r.task_id.clone()).collect();
        let page2_ids: std::collections::HashSet<String> =
            page2.items.iter().map(|r| r.task_id.clone()).collect();
        let overlap: Vec<&String> = page1_ids.intersection(&page2_ids).collect();
        assert!(
            overlap.is_empty(),
            "cursor pagination duplicated rows: {overlap:?}"
        );
    }

    #[test]
    fn list_cursor_with_status_filter() {
        let s = store();
        let t1 = mk(&s, "a", "f", "{}", "o");
        let _t2 = mk(&s, "b", "f", "{}", "o");
        let t3 = mk(&s, "c", "f", "{}", "o");
        s.update(&t1, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(&t3, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let page = s.list_cursor(None, 100, Some("running")).unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(page.items.iter().all(|r| r.status == "running"));
    }

    #[test]
    fn list_cursor_empty_page_returns_none_cursor() {
        let s = store();
        let page = s.list_cursor(None, 10, None).unwrap();
        assert!(page.items.is_empty());
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn task_cursor_encode_parse_roundtrip() {
        let c = TaskCursor {
            updated_at: 1_700_000_000,
            task_id: "0123456789abcdef0123456789abcdef".into(),
        };
        let s = c.encode();
        let back = TaskCursor::parse(&s).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn task_cursor_parse_malformed_is_none() {
        assert!(TaskCursor::parse("").is_none());
        assert!(TaskCursor::parse("nocolon").is_none());
        assert!(TaskCursor::parse(":onlyid").is_none());
        assert!(TaskCursor::parse("123:").is_none());
        assert!(TaskCursor::parse("notanumber:abc").is_none());
    }

    // ── Priority A (continuation): chronicle ergonomics ───────────────

    #[test]
    fn query_events_type_filter_matches_exact() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.append_event(&tid, "task.attempt_started", "a1").unwrap();
        s.append_event(&tid, "task.attempt_finished", "a1f")
            .unwrap();
        s.append_event(&tid, "task.attempt_started", "a2").unwrap();
        let started = s
            .query_events(&tid, 0, 100, Some("task.attempt_started"), EventOrder::Asc)
            .unwrap();
        assert_eq!(started.len(), 2);
        assert!(
            started
                .iter()
                .all(|e| e.event_type == "task.attempt_started")
        );
        let finished = s
            .query_events(&tid, 0, 100, Some("task.attempt_finished"), EventOrder::Asc)
            .unwrap();
        assert_eq!(finished.len(), 1);
    }

    #[test]
    fn query_events_desc_order_returns_newest_first() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        for i in 0..5 {
            s.append_event(&tid, "step", &format!("e{i}")).unwrap();
        }
        let desc = s.query_events(&tid, 0, 3, None, EventOrder::Desc).unwrap();
        assert_eq!(desc.len(), 3);
        // Newest first → payloads should be e4, e3, e2.
        assert_eq!(desc[0].payload, "e4");
        assert_eq!(desc[1].payload, "e3");
        assert_eq!(desc[2].payload, "e2");
    }

    #[test]
    fn query_events_empty_type_filter_equals_no_filter() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.append_event(&tid, "a", "1").unwrap();
        s.append_event(&tid, "b", "2").unwrap();
        let empty = s
            .query_events(&tid, 0, 100, Some(""), EventOrder::Asc)
            .unwrap();
        let none = s.query_events(&tid, 0, 100, None, EventOrder::Asc).unwrap();
        assert_eq!(empty.len(), none.len());
        assert_eq!(empty.len(), 2);
    }

    #[test]
    fn query_events_type_no_match_returns_empty() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.append_event(&tid, "step", "x").unwrap();
        let v = s
            .query_events(&tid, 0, 100, Some("nope.no.match"), EventOrder::Asc)
            .unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn event_order_parse_roundtrip() {
        assert_eq!(EventOrder::parse("asc"), Some(EventOrder::Asc));
        assert_eq!(EventOrder::parse(""), Some(EventOrder::Asc));
        assert_eq!(EventOrder::parse("desc"), Some(EventOrder::Desc));
        assert!(EventOrder::parse("sideways").is_none());
        assert_eq!(EventOrder::Asc.as_str(), "asc");
        assert_eq!(EventOrder::Desc.as_str(), "desc");
    }

    // ── Priority F hardening: pagination edge cases ───────────────────

    #[test]
    fn list_paginated_offset_past_end_returns_empty() {
        let s = store();
        for i in 0..3 {
            mk(&s, &format!("t{i}"), "f", "{}", "o");
        }
        let v = s.list_paginated(50, 99, None).unwrap();
        assert!(v.is_empty(), "offset past end must return empty, got {v:?}");
    }

    #[test]
    fn list_paginated_huge_limit_is_clamped() {
        let s = store();
        for i in 0..5 {
            mk(&s, &format!("t{i}"), "f", "{}", "o");
        }
        // Request u32::MAX rows; expect at most max_list rows but in
        // practice limited by what exists. Verify it doesn't OOM or
        // panic.
        let v = s.list_paginated(usize::MAX / 2, 0, None).unwrap();
        assert_eq!(v.len(), 5);
    }

    #[test]
    fn list_events_after_huge_limit_is_clamped() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        for i in 0..3 {
            s.append_event(&tid, "step", &format!("e{i}")).unwrap();
        }
        let v = s.list_events_after(&tid, 0, usize::MAX / 2).unwrap();
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn list_events_after_negative_after_id_treats_as_zero() {
        // The SELECT condition is `event_id > ?`. SQLite treats a
        // negative integer literally — every positive event_id is
        // greater than -100, so the call returns everything. Verify.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.append_event(&tid, "step", "a").unwrap();
        let v = s.list_events_after(&tid, -100, 50).unwrap();
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn count_with_unknown_status_returns_zero_not_error() {
        let s = store();
        mk(&s, "t", "f", "{}", "o");
        // The Coordinator does not validate status against an enum —
        // operators can write any string. Asking for the count of a
        // status that doesn't appear must return 0, not error.
        let n = s.count(Some("definitely_not_a_real_status")).unwrap();
        assert_eq!(n, 0);
    }

    // ── Priority H: lightweight scalability smoke checks ─────────────

    #[test]
    fn list_paginated_handles_thousand_tasks_quickly() {
        // Regression guard for accidental O(N²) behaviour in any
        // future filter / join. Creates 1000 tasks; expects list +
        // count + paginated walk to complete well under a second.
        let s = TaskStore::in_memory().expect("open");
        let start = std::time::Instant::now();
        for i in 0..1000 {
            s.create(&format!("t{i}"), "f", "{}", "o", RetryPolicy::None, 0, None)
                .unwrap();
        }
        let create_elapsed = start.elapsed();
        // Counts.
        let now = std::time::Instant::now();
        assert_eq!(s.count(None).unwrap(), 1000);
        assert_eq!(s.count(Some("pending")).unwrap(), 1000);
        let count_elapsed = now.elapsed();
        // Walk pages of 100 with offset.
        let now = std::time::Instant::now();
        let mut total = 0;
        for off in (0..1000).step_by(100) {
            let page = s.list_paginated(100, off, None).unwrap();
            total += page.len();
        }
        let walk_elapsed = now.elapsed();
        assert_eq!(total, 1000);
        // Generous bounds: anything > 5s here is a real regression.
        assert!(
            create_elapsed.as_secs() < 5,
            "create 1000 took {create_elapsed:?}"
        );
        assert!(
            count_elapsed.as_millis() < 500,
            "count 2x took {count_elapsed:?}"
        );
        assert!(
            walk_elapsed.as_millis() < 500,
            "paginated walk took {walk_elapsed:?}"
        );
    }

    #[test]
    fn list_paginated_handles_ten_thousand_tasks_quickly() {
        // S6 scale check. 10x the existing 1000-task smoke. Catches
        // regressions where a quadratic or per-row allocation
        // sneaks in. Bounds are deliberately loose — real perf
        // work waits until there's a load profile to optimise for.
        let s = TaskStore::in_memory().expect("open");
        let start = std::time::Instant::now();
        // Use larger max_list so the 1000-row pages don't get clamped.
        // Inline `create` is the bottleneck; bound the whole test at
        // 30s on a typical dev machine.
        for i in 0..10_000 {
            s.create(&format!("t{i}"), "f", "{}", "o", RetryPolicy::None, 0, None)
                .unwrap();
        }
        let create = start.elapsed();
        assert!(create.as_secs() < 30, "create 10k tasks took {create:?}");

        let now = std::time::Instant::now();
        assert_eq!(s.count(None).unwrap(), 10_000);
        let count_elapsed = now.elapsed();
        assert!(
            count_elapsed.as_millis() < 500,
            "count 10k took {count_elapsed:?}"
        );

        // Walk via cursor — the production primitive at scale.
        let now = std::time::Instant::now();
        let mut cursor = None;
        let mut total = 0usize;
        loop {
            let page = s.list_cursor(cursor.clone(), 200, None).unwrap();
            if page.items.is_empty() {
                break;
            }
            total += page.items.len();
            cursor = page.next_cursor;
        }
        let walk = now.elapsed();
        assert_eq!(total, 10_000);
        // 50 pages of 200; should be well under 3s on a dev machine.
        assert!(walk.as_secs() < 3, "cursor walk of 10k tasks took {walk:?}");
    }

    #[test]
    fn list_events_after_handles_ten_thousand_events_quickly() {
        // S6: scale check for chronicle pagination. 10K events,
        // walked in pages of 500.
        let s = TaskStore::in_memory().expect("open");
        let tid = mk(&s, "t", "f", "{}", "o");
        let start = std::time::Instant::now();
        for i in 0..10_000 {
            s.append_event(&tid, "step", &format!("e{i}")).unwrap();
        }
        let append = start.elapsed();
        assert!(append.as_secs() < 30, "append 10k events took {append:?}");

        let now = std::time::Instant::now();
        let mut after = 0i64;
        let mut total = 0usize;
        loop {
            let chunk = s.list_events_after(&tid, after, 500).unwrap();
            if chunk.is_empty() {
                break;
            }
            after = chunk.last().unwrap().event_id;
            total += chunk.len();
        }
        let walk = now.elapsed();
        assert_eq!(total, 10_000);
        assert!(
            walk.as_secs() < 3,
            "incremental walk of 10k events took {walk:?}"
        );
    }

    #[test]
    fn count_compact_candidates_stays_fast_on_ten_thousand_events() {
        // Scale check for the chronicle-retention dry-run
        // count. The query JOINs task_events to tasks and
        // filters by status — both load-bearing for operators
        // running this on a live ledger. Establish the budget
        // here so future regressions surface in CI rather than
        // production.
        let s = TaskStore::in_memory().expect("open");
        // Mixed cohort: 200 tasks each with 50 events. Mark
        // half completed (terminal — counted), the other half
        // running (R5-excluded). Should yield 100 * 50 = 5000
        // candidates out of 10_000 total events.
        let mut ids = Vec::with_capacity(200);
        for i in 0..200 {
            ids.push(mk(&s, &format!("t{i}"), "f", "{}", "o"));
        }
        for tid in &ids {
            for j in 0..50 {
                s.append_event(tid, "step", &format!("e{j}")).unwrap();
            }
        }
        for (i, tid) in ids.iter().enumerate() {
            let st = if i % 2 == 0 { "completed" } else { "running" };
            s.update(tid, Some(st), None, None, None, None, None, None)
                .unwrap();
        }
        // Cutoff = "future" so every existing event qualifies.
        let cutoff = unix_secs() + 60;
        let now = std::time::Instant::now();
        let r = s.count_compact_candidates(cutoff).unwrap();
        let elapsed = now.elapsed();
        assert_eq!(
            r.candidate_events, 5000,
            "expected 5000 candidates (100 completed * 50 events)"
        );
        assert_eq!(r.candidate_tasks, 100);
        // 500ms budget on a dev machine. The SQL is two
        // aggregate queries against indexed columns; anything
        // close to this ceiling is the sign of a regression
        // (missing index, table scan, etc.).
        assert!(
            elapsed.as_millis() < 500,
            "compact dry-run on 10k events took {elapsed:?}; budget is 500ms"
        );
    }

    #[test]
    fn list_events_after_handles_large_chronicle_quickly() {
        // 5000 events on one task. Read in pages of 500.
        let s = TaskStore::in_memory().expect("open");
        let tid = mk(&s, "t", "f", "{}", "o");
        for i in 0..5000 {
            s.append_event(&tid, "step", &format!("e{i}")).unwrap();
        }
        let mut after = 0i64;
        let mut total = 0usize;
        let now = std::time::Instant::now();
        loop {
            let chunk = s.list_events_after(&tid, after, 500).unwrap();
            if chunk.is_empty() {
                break;
            }
            after = chunk.last().unwrap().event_id;
            total += chunk.len();
        }
        let elapsed = now.elapsed();
        assert_eq!(total, 5000);
        assert!(
            elapsed.as_secs() < 5,
            "incremental 5000-event walk took {elapsed:?}"
        );
    }

    #[test]
    fn investigation_marker_mark_clear_round_trip() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        // Initially unset.
        let v0 = s.get(&tid).unwrap().unwrap();
        assert!(v0.investigation_marked_at.is_none());
        assert!(v0.investigation_reason.is_none());
        // Mark with reason.
        let ts = s
            .set_investigation_marker(&tid, true, Some(" check logs "), "subj-op")
            .unwrap();
        assert!(ts.is_some());
        let v1 = s.get(&tid).unwrap().unwrap();
        assert_eq!(v1.investigation_marked_at, ts);
        assert_eq!(v1.investigation_reason.as_deref(), Some("check logs"));
        // Mark again with no reason — reason cleared.
        let ts2 = s
            .set_investigation_marker(&tid, true, None, "subj-op")
            .unwrap();
        let v2 = s.get(&tid).unwrap().unwrap();
        assert_eq!(v2.investigation_marked_at, ts2);
        assert!(v2.investigation_reason.is_none());
        // Clear.
        let cleared = s
            .set_investigation_marker(&tid, false, None, "subj-op")
            .unwrap();
        assert!(cleared.is_none());
        let v3 = s.get(&tid).unwrap().unwrap();
        assert!(v3.investigation_marked_at.is_none());
        assert!(v3.investigation_reason.is_none());
    }

    #[test]
    fn investigation_marker_emits_chronicle_events() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.set_investigation_marker(&tid, true, Some("triage"), "op")
            .unwrap();
        s.set_investigation_marker(&tid, false, None, "op").unwrap();
        let events = s.list_events_after(&tid, 0, 50).unwrap();
        let marked = events
            .iter()
            .find(|e| e.event_type == "task.investigation_marked")
            .expect("marked event missing");
        assert!(marked.payload.contains("marked"));
        let pj: serde_json::Value =
            serde_json::from_str(marked.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(pj["marked"], true);
        assert_eq!(pj["reason"], "triage");
        assert_eq!(pj["author"], "op");
        let cleared = events
            .iter()
            .find(|e| e.event_type == "task.investigation_cleared")
            .expect("cleared event missing");
        assert_eq!(cleared.payload, "cleared");
    }

    #[test]
    fn investigation_marker_rejects_unknown_task() {
        let s = store();
        let err = s
            .set_investigation_marker("deadbeef", true, None, "op")
            .unwrap_err();
        assert!(matches!(err, CoordinatorError::NotFound(id) if id == "deadbeef"));
    }

    #[test]
    fn investigation_marker_rejects_oversize_reason() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let big = "x".repeat(MAX_OPERATOR_NOTE_LEN + 1);
        let err = s
            .set_investigation_marker(&tid, true, Some(&big), "op")
            .unwrap_err();
        assert!(matches!(err, CoordinatorError::Invalid(_)));
    }

    #[test]
    fn append_operator_note_writes_typed_event() {
        // M60: the note lands as a typed `task.operator_note`
        // event with payload_json carrying author + note. The
        // legacy `payload` string carries the trimmed note so
        // older grep-driven CLIs see it directly.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let event_id = s
            .append_operator_note(&tid, "  investigate after lunch  ", "subj-anshul")
            .unwrap();
        assert!(event_id > 0);
        let events = s.list_events_after(&tid, 0, 50).unwrap();
        let note_ev = events
            .iter()
            .find(|e| e.event_type == "task.operator_note")
            .expect("note event missing");
        assert_eq!(note_ev.payload, "investigate after lunch");
        let pj = note_ev
            .payload_json
            .as_deref()
            .expect("payload_json should be populated");
        let parsed: serde_json::Value = serde_json::from_str(pj).unwrap();
        assert_eq!(parsed["note"], "investigate after lunch");
        assert_eq!(parsed["author"], "subj-anshul");
    }

    #[test]
    fn append_operator_note_rejects_empty_and_oversize_input() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        // Empty after trim.
        let err = s.append_operator_note(&tid, "   ", "sub").unwrap_err();
        assert!(matches!(err, CoordinatorError::Invalid(_)));
        // Oversize.
        let big = "a".repeat(MAX_OPERATOR_NOTE_LEN + 1);
        let err = s.append_operator_note(&tid, &big, "sub").unwrap_err();
        assert!(matches!(err, CoordinatorError::Invalid(_)));
    }

    #[test]
    fn append_operator_note_rejects_unknown_task_id() {
        let s = store();
        let err = s
            .append_operator_note("deadbeef", "valid note", "sub")
            .unwrap_err();
        assert!(matches!(err, CoordinatorError::NotFound(id) if id == "deadbeef"));
    }

    #[test]
    fn append_operator_note_preserves_pipe_chars_in_text() {
        // The wire format is `task_id|note`, but the bridge
        // splits with splitn(2), so the note may contain `|`.
        // The append helper takes a plain &str — verify the
        // store doesn't sanitise pipes out of the recorded
        // value.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let note = "key=value | other=thing";
        s.append_operator_note(&tid, note, "sub").unwrap();
        let events = s.list_events_after(&tid, 0, 50).unwrap();
        let note_ev = events
            .iter()
            .find(|e| e.event_type == "task.operator_note")
            .expect("note event missing");
        assert_eq!(note_ev.payload, note);
    }

    #[test]
    fn append_event_with_special_chars_does_not_break_chronicle() {
        // Operator-defined events may contain payload chars that
        // would break naive rendering. Verify they survive
        // store -> list_events_after.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let payload = "key=\"value\" key2=val with spaces\tand\ttabs\nand newlines";
        s.append_event(&tid, "ops.custom", payload).unwrap();
        let v = s.list_events_after(&tid, 0, 50).unwrap();
        assert_eq!(v.len(), 1);
        // The Coordinator stores the payload verbatim (escaping is
        // the renderer's concern, not the store's).
        assert_eq!(v[0].payload, payload);
    }

    #[test]
    fn list_backward_compat_old_signature_still_works() {
        // The old `list(limit)` method must keep behaving like the
        // paginated version with offset=0 and no filter — the bridge
        // and CLI both call it.
        let s = store();
        mk(&s, "a", "f", "{}", "o");
        mk(&s, "b", "f", "{}", "o");
        let v = s.list(10).unwrap();
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn task_id_collision_resistance_check() {
        // 1000 generated IDs should all be unique (32-hex from
        // OsRng — collision probability is negligible but the test
        // catches a future regression where new_task_id is replaced
        // with something deterministic).
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = new_task_id();
            assert_eq!(id.len(), 32);
            assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(seen.insert(id), "collision");
        }
    }

    // ── Chronicle retention Step 2: dry-run compact counter ───────────

    /// Build a small fixture: 3 terminal-state tasks each with
    /// some events, plus one in-flight (running) task with
    /// events. Returns the cutoff_ts that admits all-but-the-
    /// last event of each task.
    fn compact_fixture() -> (TaskStore, i64) {
        let s = store();
        // Two completed, one failed, one cancelled, plus one
        // running (R5 guard target).
        let completed_a = mk(&s, "ca", "f", "{}", "o");
        let completed_b = mk(&s, "cb", "f", "{}", "o");
        let failed_x = mk(&s, "fx", "f", "{}", "o");
        let cancelled_y = mk(&s, "cy", "f", "{}", "o");
        let running_r = mk(&s, "rr", "f", "{}", "o");
        for tid in [
            &completed_a,
            &completed_b,
            &failed_x,
            &cancelled_y,
            &running_r,
        ] {
            for i in 0..3 {
                s.append_event(tid, "ops.test", &format!("e{i}")).unwrap();
            }
        }
        for (tid, st) in [
            (&completed_a, "completed"),
            (&completed_b, "completed"),
            (&failed_x, "failed"),
            (&cancelled_y, "cancelled"),
            (&running_r, "running"),
        ] {
            s.update(tid, Some(st), None, None, None, None, None, None)
                .unwrap();
        }
        // Cutoff = "future" so every existing event is older.
        let cutoff = unix_secs() + 60;
        (s, cutoff)
    }

    #[test]
    fn compact_dry_run_counts_only_terminal_state_tasks() {
        // R5 invariant: events belonging to a `running` task
        // must not appear in the candidate count. The fixture
        // has 5 tasks * 3 events each, but only the 4 terminal
        // tasks contribute (= 12 events). The running task
        // (3 events) is excluded.
        let (s, cutoff) = compact_fixture();
        let r = s.count_compact_candidates(cutoff).unwrap();
        assert_eq!(
            r.candidate_events, 12,
            "candidate_events should be 4 terminal tasks × 3 events"
        );
        assert_eq!(
            r.candidate_tasks, 4,
            "running task must not contribute to candidate_tasks"
        );
        assert_eq!(r.cutoff_ts, cutoff);
        assert!(r.oldest_candidate_ts.is_some());
        assert!(r.newest_candidate_ts.is_some());
    }

    #[test]
    fn compact_dry_run_breakdown_groups_by_terminal_status() {
        let (s, cutoff) = compact_fixture();
        let r = s.count_compact_candidates(cutoff).unwrap();
        // Alphabetical sort: cancelled, completed, failed.
        // The `running` cohort must be absent (R5).
        let m: std::collections::HashMap<String, i64> = r.by_task_status.into_iter().collect();
        assert_eq!(m.get("completed").copied(), Some(6)); // 2 tasks × 3 events
        assert_eq!(m.get("failed").copied(), Some(3));
        assert_eq!(m.get("cancelled").copied(), Some(3));
        assert!(
            !m.contains_key("running"),
            "running cohort must not appear in compact candidates (R5)"
        );
    }

    #[test]
    fn compact_dry_run_empty_when_cutoff_before_any_event() {
        // Cutoff well in the past — no events should match.
        let (s, _) = compact_fixture();
        let r = s.count_compact_candidates(0).unwrap();
        assert_eq!(r.candidate_events, 0);
        assert_eq!(r.candidate_tasks, 0);
        assert!(r.oldest_candidate_ts.is_none());
        assert!(r.newest_candidate_ts.is_none());
        assert!(r.by_task_status.is_empty());
    }

    #[test]
    fn render_compact_dry_run_is_valid_json_with_expected_fields() {
        let (s, cutoff) = compact_fixture();
        let r = s.count_compact_candidates(cutoff).unwrap();
        let body = render_compact_dry_run(&r, "dry-run");
        let parsed: serde_json::Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("dry-run body did not parse:\n{body}\nerror: {e}"));
        assert_eq!(parsed.get("mode").and_then(|v| v.as_str()), Some("dry-run"));
        assert_eq!(
            parsed.get("destructive").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            parsed.get("candidate_events").and_then(|v| v.as_i64()),
            Some(12)
        );
        assert_eq!(
            parsed.get("candidate_tasks").and_then(|v| v.as_i64()),
            Some(4)
        );
        assert!(parsed.get("by_task_status").is_some());
    }

    #[test]
    fn render_compact_dry_run_handles_empty_breakdown_without_trailing_comma() {
        // Empty candidate set means by_task_status is `{}`. The
        // hand-built JSON would emit a trailing comma if the
        // separator logic were broken — assert it parses.
        let s = store();
        let r = s.count_compact_candidates(0).unwrap();
        let body = render_compact_dry_run(&r, "dry-run");
        let parsed: serde_json::Value = serde_json::from_str(&body)
            .unwrap_or_else(|e| panic!("empty body did not parse:\n{body}\nerror: {e}"));
        let by = parsed.get("by_task_status").unwrap();
        assert!(by.is_object());
        assert_eq!(by.as_object().unwrap().len(), 0);
        // oldest/newest should be absent when there are no
        // candidates — skip_serializing_if equivalent.
        assert!(parsed.get("oldest_candidate_ts").is_none());
        assert!(parsed.get("newest_candidate_ts").is_none());
    }

    // ── Handler-level hardening for task.compact_events ───────────────

    fn compact_ctx(args: &[u8]) -> InvocationCtx {
        use relix_core::identity::VerifiedIdentity;
        use relix_core::types::{NodeId, RequestId, TraceId};
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"op"),
                name: "op".into(),
                org_id: NodeId::from_pubkey(b"org"),
                groups: vec!["operators".into()],
                role: "operator".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: args.to_vec(),
        }
    }

    fn compact_err_cause(outcome: HandlerOutcome) -> String {
        match outcome {
            HandlerOutcome::Err(d) => d.cause,
            _ => panic!("expected HandlerOutcome::Err"),
        }
    }

    #[test]
    fn compact_handler_rejects_empty_args() {
        let s = store();
        let cause = compact_err_cause(handle_compact_events(&s, &compact_ctx(b"")));
        assert!(
            cause.contains("max_age_secs required"),
            "unexpected cause: {cause}"
        );
    }

    #[test]
    fn compact_handler_rejects_negative_max_age() {
        let s = store();
        let cause = compact_err_cause(handle_compact_events(&s, &compact_ctx(b"-1|dry-run")));
        assert!(
            cause.contains("bad max_age_secs"),
            "unexpected cause: {cause}"
        );
    }

    #[test]
    fn compact_handler_rejects_zero_max_age() {
        // Zero is meaningless — no events would ever match.
        // Reject explicitly instead of returning an empty
        // result that's indistinguishable from a real
        // "nothing matches" outcome.
        let s = store();
        let cause = compact_err_cause(handle_compact_events(&s, &compact_ctx(b"0|dry-run")));
        assert!(
            cause.contains("bad max_age_secs"),
            "unexpected cause: {cause}"
        );
    }

    #[test]
    fn compact_handler_rejects_nonnumeric_max_age() {
        let s = store();
        let cause = compact_err_cause(handle_compact_events(&s, &compact_ctx(b"never|dry-run")));
        assert!(
            cause.contains("bad max_age_secs"),
            "unexpected cause: {cause}"
        );
    }

    #[test]
    fn compact_handler_rejects_destructive_mode() {
        // The whole point of the mode guard: a future caller
        // who passes mode=delete must get a clear "not
        // implemented" error, not silent acceptance as a
        // dry-run.
        let s = store();
        let cause = compact_err_cause(handle_compact_events(&s, &compact_ctx(b"60|delete")));
        assert!(
            cause.contains("not implemented") && cause.contains("dry-run"),
            "unexpected cause: {cause}"
        );
    }

    #[test]
    fn compact_handler_accepts_dry_run_or_default_mode() {
        // Default mode (omitted) and explicit dry-run both
        // succeed and return the same wire shape.
        let s = store();
        let r1 = match handle_compact_events(&s, &compact_ctx(b"3600")) {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            _ => panic!("expected HandlerOutcome::Ok"),
        };
        let r2 = match handle_compact_events(&s, &compact_ctx(b"3600|dry-run")) {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            _ => panic!("expected HandlerOutcome::Ok"),
        };
        for body in [&r1, &r2] {
            let v: serde_json::Value = serde_json::from_str(body)
                .unwrap_or_else(|e| panic!("body did not parse:\n{body}\nerror: {e}"));
            assert_eq!(v.get("mode").and_then(|x| x.as_str()), Some("dry-run"));
            assert_eq!(v.get("destructive").and_then(|x| x.as_bool()), Some(false));
        }
    }

    // ── Phase-1E M38: execution edges ────────────────────────────────

    #[test]
    fn no_edges_recorded_for_a_single_attempt_task() {
        // A task that runs once and completes has no retry chain
        // → no retried_from edge should exist.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
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
        let edges = s.list_edges_for_task(&tid).unwrap();
        assert!(edges.is_empty(), "expected no edges, got: {edges:?}");
    }

    #[test]
    fn retry_creates_retried_from_edge_pointing_at_prior_attempt() {
        // Real causality: attempt 2 was created BECAUSE attempt
        // 1 failed and the operator called retry. The edge
        // points attempt 2 → attempt 1 with edge_type
        // retried_from + the chronicle event_id of the
        // task.retry_requested as the trigger.
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, None)
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        // Fail attempt 1.
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
        // Request retry → emits task.retry_requested.
        let decision = s.request_retry(&tid).unwrap();
        match decision {
            RetryDecision::Accepted { .. } => {}
            other => panic!("expected Accepted, got {other:?}"),
        }
        // Open attempt 2 — this is where the edge gets inserted.
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();

        let attempts = s.list_attempts(&tid).unwrap();
        assert_eq!(attempts.len(), 2);
        let attempt1_id = attempts[0].attempt_id;
        let attempt2_id = attempts[1].attempt_id;

        let edges = s.list_edges_for_task(&tid).unwrap();
        assert_eq!(
            edges.len(),
            1,
            "expected one retried_from edge, got: {edges:?}"
        );
        let e = &edges[0];
        assert_eq!(e.edge_type, "retried_from");
        assert_eq!(e.task_id, tid);
        assert_eq!(e.attempt_id, Some(attempt2_id));
        assert_eq!(e.related_task_id.as_deref(), Some(tid.as_str()));
        assert_eq!(e.related_attempt_id, Some(attempt1_id));
        // The trigger event must be the task.retry_requested
        // we emitted in request_retry.
        assert!(
            e.spawned_by_event_id.is_some(),
            "expected spawned_by_event_id to reference task.retry_requested"
        );
        let v = s.get(&tid).unwrap().unwrap();
        let req_event = v
            .events
            .iter()
            .find(|ev| ev.event_type == "task.retry_requested")
            .expect("task.retry_requested must be in chronicle");
        assert_eq!(e.spawned_by_event_id, Some(req_event.event_id));
    }

    #[test]
    fn list_recent_edges_returns_newest_first_across_tasks() {
        // Set up two tasks each with a retry chain so we have
        // edges across multiple tasks. The cross-task aggregate
        // should return all of them, newest-first by edge_id.
        let s = store();
        for label in ["a", "b"] {
            let tid = s
                .create(label, "f", "{}", "o", RetryPolicy::Bounded, 3, None)
                .unwrap();
            s.update(&tid, Some("running"), None, None, None, None, None, None)
                .unwrap();
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
            let _ = s.request_retry(&tid);
            s.update(&tid, Some("running"), None, None, None, None, None, None)
                .unwrap();
        }
        let recent = s.list_recent_edges(0, 50).unwrap();
        assert_eq!(recent.len(), 2);
        // Newest first: the second task's edge has the higher
        // edge_id.
        assert!(recent[0].edge_id > recent[1].edge_id);
        // Both edges should be retried_from (no other types
        // are emitted today).
        assert!(recent.iter().all(|e| e.edge_type == "retried_from"));
    }

    #[test]
    fn list_recent_edges_honours_since_cursor() {
        let s = store();
        for _ in 0..3 {
            let tid = s
                .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, None)
                .unwrap();
            s.update(&tid, Some("running"), None, None, None, None, None, None)
                .unwrap();
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
            let _ = s.request_retry(&tid);
            s.update(&tid, Some("running"), None, None, None, None, None, None)
                .unwrap();
        }
        let all = s.list_recent_edges(0, 50).unwrap();
        assert_eq!(all.len(), 3);
        // Cursor at the middle edge → only newer edges should
        // return.
        let middle_id = all[1].edge_id;
        let after = s.list_recent_edges(middle_id, 50).unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].edge_id, all[0].edge_id);
    }

    #[test]
    fn retry_chain_creates_one_edge_per_retry_in_order() {
        // Three attempts → two retried_from edges. Each edge's
        // related_attempt_id points back at the immediately
        // prior attempt; chain reads linearly.
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 5, None)
            .unwrap();
        for _ in 0..3 {
            s.update(&tid, Some("running"), None, None, None, None, None, None)
                .unwrap();
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
            let _ = s.request_retry(&tid);
        }
        // Final open attempt (attempt 4 wouldn't open without a
        // running transition; we want exactly 3 attempts in the
        // chain so DON'T open a 4th).
        // ↑ Actually the loop above pushed retry 3 times after
        // the failure of attempt 3, plus opened attempts 1/2/3.
        // The 3rd retry transitioned status to retrying but no
        // new attempt opened. So we have 3 attempts + 2 edges
        // (between 2↔1, 3↔2).

        let attempts = s.list_attempts(&tid).unwrap();
        assert_eq!(attempts.len(), 3, "expected 3 attempts");
        let edges = s.list_edges_for_task(&tid).unwrap();
        assert_eq!(
            edges.len(),
            2,
            "expected 2 retried_from edges for 3 attempts"
        );
        // Edge 0: attempt 2 → attempt 1
        assert_eq!(edges[0].edge_type, "retried_from");
        assert_eq!(edges[0].attempt_id, Some(attempts[1].attempt_id));
        assert_eq!(edges[0].related_attempt_id, Some(attempts[0].attempt_id));
        // Edge 1: attempt 3 → attempt 2
        assert_eq!(edges[1].edge_type, "retried_from");
        assert_eq!(edges[1].attempt_id, Some(attempts[2].attempt_id));
        assert_eq!(edges[1].related_attempt_id, Some(attempts[1].attempt_id));
    }
}
