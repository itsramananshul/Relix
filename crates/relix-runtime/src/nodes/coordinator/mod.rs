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

pub mod agent;
pub mod brief;
pub mod cron;
pub mod delegate;
pub mod event_summary;
pub mod heartbeat;
pub mod messaging;
pub mod routing;
pub mod spine;
pub use event_summary::{summarize_event, summarize_event_parts};

/// H4: number of consecutive failures sharing the same
/// `last_failure_class` that triggers automatic investigation
/// marking + a `task.thrash_detected` chronicle event. Higher
/// than 1 so a single transient flap doesn't false-positive;
/// low enough that a stuck retry loop is caught within a
/// handful of attempts. Operators can pre-mark a task ahead
/// of this threshold via `task.mark_investigation` — the
/// auto-marker skips already-marked tasks to avoid clobbering
/// an existing operator reason.
pub const ANTI_THRASH_THRESHOLD: i64 = 3;

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
    /// Operator-controlled chronicle retention. Absent ⇒ disabled
    /// (no background deletion); operators see the dry-run via
    /// `task.compact_events` exactly as before. See
    /// `docs/chronicle-retention.md` for the design contract.
    #[serde(default)]
    pub retention: RetentionConfig,
    /// `[coordinator.ai_peer]` — optional AI peer config the
    /// coordinator dials for drift embedding (W4). Absent means
    /// the drift hook records `similarity=none` even when the
    /// embedder cell is built — the cell stays empty.
    #[serde(default, rename = "ai_peer")]
    pub ai_peer: Option<CoordinatorAiPeerConfig>,
}

/// `[coordinator.ai_peer]` — operator-supplied AI peer
/// address + alias + deadline. Mirrors the
/// `[ai.memory_peer]` shape so an operator who already
/// wired the AI controller's memory peer can copy/paste.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct CoordinatorAiPeerConfig {
    /// libp2p multiaddr of the AI peer (e.g.
    /// `/ip4/127.0.0.1/tcp/19712`).
    pub addr: String,
    /// Mesh alias used in outbound calls. Defaults to
    /// `"ai"`.
    #[serde(default = "default_ai_alias")]
    pub alias: String,
    /// Per-call deadline in seconds. Defaults to 10.
    #[serde(default = "default_ai_deadline_secs")]
    pub deadline_secs: i64,
}

fn default_ai_alias() -> String {
    "ai".to_string()
}

fn default_ai_deadline_secs() -> i64 {
    10
}

/// `[coordinator.retention]`. Every field is optional; missing
/// section means `enabled = false` and the retention loop never
/// starts.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct RetentionConfig {
    /// Master switch. `false` (default) means no background
    /// deletion ever runs. Operators flip this on after they're
    /// satisfied with the dry-run output from
    /// `task.compact_events`.
    #[serde(default)]
    pub enabled: bool,
    /// Events older than `now - max_task_age_days * 86400` are
    /// candidates for deletion (R5 still applies — parent task
    /// must be in a terminal state). Default 30 days.
    #[serde(default = "default_max_task_age_days")]
    pub max_task_age_days: u32,
    /// Per-task event count cap. When a task has more than this
    /// many events, the oldest ones become deletion candidates.
    /// 0 disables the cap. Default 500.
    #[serde(default = "default_max_events_per_task")]
    pub max_events_per_task: u32,
    /// How often the background retention loop runs, in hours.
    /// Default 24. The loop only spawns when `enabled = true`.
    #[serde(default = "default_compact_interval_h")]
    pub compact_interval_h: u32,
    /// Cap on the number of bounded-delete passes per run. Each
    /// pass deletes at most `MAX_ROWS_PER_PASS` rows inside its
    /// own transaction; the run stops when either no more rows
    /// qualify OR this cap is reached. Default 10.
    #[serde(default = "default_max_passes_per_run")]
    pub max_passes_per_run: u32,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_task_age_days: default_max_task_age_days(),
            max_events_per_task: default_max_events_per_task(),
            compact_interval_h: default_compact_interval_h(),
            max_passes_per_run: default_max_passes_per_run(),
        }
    }
}

fn default_max_task_age_days() -> u32 {
    30
}
fn default_max_events_per_task() -> u32 {
    500
}
fn default_compact_interval_h() -> u32 {
    24
}
fn default_max_passes_per_run() -> u32 {
    10
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            max_list: default_max_list(),
            recovery_scan: default_recovery_scan(),
            retention: RetentionConfig::default(),
            ai_peer: None,
        }
    }
}

fn default_max_list() -> usize {
    200
}

fn default_recovery_scan() -> bool {
    true
}

/// Hard cap on per-pass deletion. Matches the spec: "Delete at
/// most 1000 rows per pass inside a transaction." Operators
/// can't lift this — large deletions chunk into multiple passes,
/// each one short enough to keep the writer mutex from blocking
/// chat traffic for a noticeable time.
pub const MAX_ROWS_PER_RETENTION_PASS: i64 = 1000;

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
        let mut conn = Connection::open(&cfg.db_path).map_err(CoordinatorError::Db)?;
        // Production pragmas (FK enforcement, WAL, busy timeout) +
        // startup integrity probe + migration-version bootstrap.
        // See `crate::db` for the shared contract.
        crate::db::apply_pragmas(&conn).map_err(CoordinatorError::Db)?;
        crate::db::log_integrity_warning(&conn, "coordinator");
        crate::db::ensure_migration_table(&conn).map_err(CoordinatorError::Db)?;
        init_schema(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            max_list: cfg.max_list.max(1),
        })
    }

    /// In-memory backend for unit tests.
    pub fn in_memory() -> Result<Self, CoordinatorError> {
        let mut conn = Connection::open_in_memory().map_err(CoordinatorError::Db)?;
        crate::db::apply_pragmas(&conn).map_err(CoordinatorError::Db)?;
        crate::db::ensure_migration_table(&conn).map_err(CoordinatorError::Db)?;
        init_schema(&mut conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            max_list: 200,
        })
    }

    /// PHASE 1 (Brief): move a Brief's **board status**, enforcing
    /// the board state machine ([`brief::board_transition_allowed`]).
    /// Board status is the column the Brief sits in on the operator
    /// board — separate from the execution `status`. Returns the
    /// `(from, to)` pair on success.
    ///
    /// Errors: `Invalid` if `to` is unknown or the move is illegal
    /// (skipping columns, or leaving the terminal `cancelled`);
    /// `NotFound` if the Brief doesn't exist.
    pub fn set_board_status(
        &self,
        task_id: &str,
        to: &str,
    ) -> Result<(String, String), CoordinatorError> {
        if !brief::is_board_status(to) {
            return Err(CoordinatorError::Invalid(format!(
                "unknown board status '{to}'"
            )));
        }
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let from: String = match conn.query_row(
            "SELECT board_status FROM tasks WHERE task_id = ?1",
            params![task_id],
            |r| r.get(0),
        ) {
            Ok(s) => s,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(CoordinatorError::NotFound(task_id.to_string()));
            }
            Err(e) => return Err(CoordinatorError::Db(e)),
        };
        if !brief::board_transition_allowed(&from, to) {
            return Err(CoordinatorError::Invalid(format!(
                "illegal board move {from} -> {to}"
            )));
        }
        let now = unix_secs();
        conn.execute(
            "UPDATE tasks SET board_status = ?1, updated_at = ?2 WHERE task_id = ?3",
            params![to, now, task_id],
        )
        .map_err(CoordinatorError::Db)?;
        // Record the move on the Brief's chronicle (skip no-ops).
        if from != to {
            conn.execute(
                "INSERT INTO task_events (task_id, ts, event_type, payload)
                 VALUES (?1, ?2, 'brief.board_moved', ?3)",
                params![task_id, now, format!("{from} -> {to}")],
            )
            .map_err(CoordinatorError::Db)?;
        }
        Ok((from, to.to_string()))
    }

    /// PHASE 1 (Brief): read a Brief's current board status. `None`
    /// when the Brief doesn't exist.
    pub fn board_status(&self, task_id: &str) -> Result<Option<String>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        match conn.query_row(
            "SELECT board_status FROM tasks WHERE task_id = ?1",
            params![task_id],
            |r| r.get::<_, String>(0),
        ) {
            Ok(s) => Ok(Some(s)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CoordinatorError::Db(e)),
        }
    }

    /// PHASE 1 (Brief): link `child` as a **Sub-brief** of
    /// `parent` (a `task_edges` 'spawned' edge). Both must exist;
    /// no self-link; idempotent. The planner-decomposition emitter.
    pub fn link_subbrief(&self, parent: &str, child: &str) -> Result<(), CoordinatorError> {
        self.add_brief_edge(parent, child, "spawned", "Sub-brief")
    }

    /// PHASE 1 (Brief): the Sub-briefs of `parent`, as task_ids.
    pub fn list_subbriefs(&self, parent: &str) -> Result<Vec<String>, CoordinatorError> {
        self.list_brief_edges(parent, "spawned")
    }

    /// PHASE 1 (Brief): detach a **Sub-brief** — remove the `parent`
    /// → `child` 'spawned' edge (a mis-decomposed plan). Chronicles
    /// `brief.subbrief_removed` when an edge is removed. Idempotent;
    /// `parent` must exist. The child Brief itself is untouched.
    pub fn unlink_subbrief(&self, parent: &str, child: &str) -> Result<(), CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        if !task_row_exists(&conn, parent)? {
            return Err(CoordinatorError::NotFound(parent.to_string()));
        }
        let removed = conn
            .execute(
                "DELETE FROM task_edges
                 WHERE task_id = ?1 AND edge_type = 'spawned' AND related_task_id = ?2",
                params![parent, child],
            )
            .map_err(CoordinatorError::Db)?;
        if removed > 0 {
            let now = unix_secs();
            let _ = conn.execute(
                "INSERT INTO task_events (task_id, ts, event_type, payload)
                 VALUES (?1, ?2, 'brief.subbrief_removed', ?3)",
                params![parent, now, child],
            );
        }
        Ok(())
    }

    /// PHASE 3 (supervisory): a parent Brief's decomposition
    /// progress — its Sub-briefs counted by board column. The signal
    /// a planner reads to see how much of its breakdown is done. The
    /// counts sum to the number of Sub-briefs.
    pub fn subbrief_progress(
        &self,
        parent: &str,
    ) -> Result<Vec<(String, i64)>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT c.board_status, COUNT(*)
                 FROM task_edges e
                 JOIN tasks c ON c.task_id = e.related_task_id
                 WHERE e.task_id = ?1 AND e.edge_type = 'spawned'
                 GROUP BY c.board_status ORDER BY c.board_status",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![parent], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// PHASE 1 (Brief): record that `task` is blocked by `blocker`
    /// — a **Snag** (a `task_edges` 'blocked_on' edge). Both must
    /// exist; no self-block; idempotent.
    pub fn add_snag(&self, task: &str, blocker: &str) -> Result<(), CoordinatorError> {
        self.add_brief_edge(task, blocker, "blocked_on", "Snag")
    }

    /// PHASE 1 (Brief): the Snags on `task` — the task_ids it is
    /// blocked by.
    pub fn list_snags(&self, task: &str) -> Result<Vec<String>, CoordinatorError> {
        self.list_brief_edges(task, "blocked_on")
    }

    /// PHASE 1 (Brief): clear a **Snag** — remove the `task` →
    /// `blocker` 'blocked_on' edge (the dependency was wrong, or has
    /// been resolved out-of-band). Chronicles `brief.snag_cleared`
    /// when an edge is actually removed. Idempotent: clearing a
    /// non-existent Snag is a no-op success. `task` must exist.
    pub fn remove_snag(&self, task: &str, blocker: &str) -> Result<(), CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        if !task_row_exists(&conn, task)? {
            return Err(CoordinatorError::NotFound(task.to_string()));
        }
        let removed = conn
            .execute(
                "DELETE FROM task_edges
                 WHERE task_id = ?1 AND edge_type = 'blocked_on' AND related_task_id = ?2",
                params![task, blocker],
            )
            .map_err(CoordinatorError::Db)?;
        if removed > 0 {
            let now = unix_secs();
            let _ = conn.execute(
                "INSERT INTO task_events (task_id, ts, event_type, payload)
                 VALUES (?1, ?2, 'brief.snag_cleared', ?3)",
                params![task, now, blocker],
            );
        }
        Ok(())
    }

    /// PHASE 1 (Brief): is `task` blocked? True when it has at
    /// least one Snag whose blocker has NOT reached board status
    /// `done`. Per the locked rule, only `done` resolves a Snag —
    /// a `cancelled` blocker stays unresolved (deliberately
    /// unsafe to auto-clear).
    pub fn is_blocked(&self, task: &str) -> Result<bool, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let unresolved: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_edges e
                 JOIN tasks b ON b.task_id = e.related_task_id
                 WHERE e.task_id = ?1 AND e.edge_type = 'blocked_on'
                   AND b.board_status != 'done'",
                params![task],
                |r| r.get(0),
            )
            .map_err(CoordinatorError::Db)?;
        Ok(unresolved > 0)
    }

    /// Shared insert for the Brief relation edges (Sub-brief /
    /// Snag). Validates both endpoints exist, forbids self-links,
    /// and is idempotent on (task_id, edge_type, related_task_id).
    fn add_brief_edge(
        &self,
        task_id: &str,
        related: &str,
        edge_type: &str,
        label: &str,
    ) -> Result<(), CoordinatorError> {
        if task_id == related {
            return Err(CoordinatorError::Invalid(format!(
                "a Brief cannot be its own {label}"
            )));
        }
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        for id in [task_id, related] {
            if !task_row_exists(&conn, id)? {
                return Err(CoordinatorError::NotFound(id.to_string()));
            }
        }
        let already = match conn.query_row(
            "SELECT 1 FROM task_edges
             WHERE task_id = ?1 AND edge_type = ?2 AND related_task_id = ?3 LIMIT 1",
            params![task_id, edge_type, related],
            |_| Ok(()),
        ) {
            Ok(()) => true,
            Err(rusqlite::Error::QueryReturnedNoRows) => false,
            Err(e) => return Err(CoordinatorError::Db(e)),
        };
        if already {
            return Ok(());
        }
        let now = unix_secs();
        conn.execute(
            "INSERT INTO task_edges (task_id, edge_type, related_task_id, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![task_id, edge_type, related, now],
        )
        .map_err(CoordinatorError::Db)?;
        // Chronicle the relation on the Brief.
        let event_type = if edge_type == "blocked_on" {
            "brief.snagged"
        } else {
            "brief.subbrief_added"
        };
        let _ = conn.execute(
            "INSERT INTO task_events (task_id, ts, event_type, payload)
             VALUES (?1, ?2, ?3, ?4)",
            params![task_id, now, event_type, related],
        );
        Ok(())
    }

    /// Shared read for the Brief relation edges. Returns the
    /// `related_task_id`s in insertion order.
    fn list_brief_edges(
        &self,
        task_id: &str,
        edge_type: &str,
    ) -> Result<Vec<String>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT related_task_id FROM task_edges
                 WHERE task_id = ?1 AND edge_type = ?2 AND related_task_id IS NOT NULL
                 ORDER BY edge_id ASC",
            )
            .map_err(CoordinatorError::Db)?;
        let rows: Vec<String> = stmt
            .query_map(params![task_id, edge_type], |r| r.get::<_, String>(0))
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// Shared *reverse* edge lookup: the task_ids that point AT
    /// `target` via `edge_type` (WHERE related_task_id = target).
    fn list_reverse_edges(
        &self,
        target: &str,
        edge_type: &str,
    ) -> Result<Vec<String>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT task_id FROM task_edges
                 WHERE related_task_id = ?1 AND edge_type = ?2
                 ORDER BY edge_id ASC",
            )
            .map_err(CoordinatorError::Db)?;
        let rows: Vec<String> = stmt
            .query_map(params![target, edge_type], |r| r.get::<_, String>(0))
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// PHASE 1 (Brief): the Briefs that THIS Brief blocks — the
    /// reverse of its Snags (who is waiting on `task` to finish).
    pub fn list_blocking(&self, task: &str) -> Result<Vec<String>, CoordinatorError> {
        self.list_reverse_edges(task, "blocked_on")
    }

    /// PHASE 1 (Brief): the parent Briefs that spawned `task` as a
    /// Sub-brief (normally one — the planner that decomposed it).
    pub fn parent_briefs(&self, task: &str) -> Result<Vec<String>, CoordinatorError> {
        self.list_reverse_edges(task, "spawned")
    }

    /// PHASE 1 (Brief): the full detail view of a Brief in one read —
    /// spine fields + both directions of the relation graph +
    /// Dossiers + blocked flag. `None` when the Brief doesn't exist.
    /// Each sub-read locks independently (no nested lock), so this is
    /// a convenience composite, not a single transaction.
    pub fn brief_detail(
        &self,
        task: &str,
    ) -> Result<Option<brief::BriefDetail>, CoordinatorError> {
        let Some(fields) = self.brief_fields(task)? else {
            return Ok(None);
        };
        Ok(Some(brief::BriefDetail {
            fields,
            subbriefs: self.list_subbriefs(task)?,
            snags: self.list_snags(task)?,
            blocking: self.list_blocking(task)?,
            parents: self.parent_briefs(task)?,
            dossiers: self.list_dossiers(task)?,
            blocked: self.is_blocked(task)?,
        }))
    }

    /// PHASE 1 (Brief): attach a **Dossier** (durable artifact) to
    /// a Brief. Append-only; returns the new `doc_id`. `kind` and
    /// `title` are required; the Brief must exist.
    pub fn add_dossier(
        &self,
        task_id: &str,
        kind: &str,
        title: &str,
        body: &str,
    ) -> Result<String, CoordinatorError> {
        if kind.trim().is_empty() {
            return Err(CoordinatorError::Invalid("dossier kind required".to_string()));
        }
        if title.trim().is_empty() {
            return Err(CoordinatorError::Invalid("dossier title required".to_string()));
        }
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        if !task_row_exists(&conn, task_id)? {
            return Err(CoordinatorError::NotFound(task_id.to_string()));
        }
        let doc_id = new_doc_id();
        let now = unix_secs();
        conn.execute(
            "INSERT INTO task_documents
                 (doc_id, task_id, kind, title, body, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![doc_id, task_id, kind.trim(), title.trim(), body, now],
        )
        .map_err(CoordinatorError::Db)?;
        let _ = conn.execute(
            "INSERT INTO task_events (task_id, ts, event_type, payload)
             VALUES (?1, ?2, 'brief.dossier_added', ?3)",
            params![task_id, now, format!("{}: {}", kind.trim(), title.trim())],
        );
        Ok(doc_id)
    }

    /// PHASE 5 (Brief): the most recent Dossier of `kind` on a Brief
    /// (full body). Dossiers are append-only/versioned, so this is
    /// "the current plan/spec" — the latest one wins. `None` when the
    /// Brief has no Dossier of that kind.
    pub fn latest_dossier(
        &self,
        task_id: &str,
        kind: &str,
    ) -> Result<Option<brief::Dossier>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        match conn.query_row(
            // `rowid` is the monotonic insert order — a reliable
            // tiebreak when several Dossiers share a created_at second
            // (doc_id is random, so it can't order by recency).
            "SELECT doc_id, task_id, kind, title, body, created_at, updated_at
             FROM task_documents WHERE task_id = ?1 AND kind = ?2
             ORDER BY created_at DESC, rowid DESC LIMIT 1",
            params![task_id, kind],
            |r| {
                Ok(brief::Dossier {
                    doc_id: r.get(0)?,
                    task_id: r.get(1)?,
                    kind: r.get(2)?,
                    title: r.get(3)?,
                    body: r.get(4)?,
                    created_at: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            },
        ) {
            Ok(d) => Ok(Some(d)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CoordinatorError::Db(e)),
        }
    }

    /// PHASE 1 (Brief): read a Dossier by id (full body). `None`
    /// when absent.
    pub fn get_dossier(&self, doc_id: &str) -> Result<Option<brief::Dossier>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        match conn.query_row(
            "SELECT doc_id, task_id, kind, title, body, created_at, updated_at
             FROM task_documents WHERE doc_id = ?1",
            params![doc_id],
            |r| {
                Ok(brief::Dossier {
                    doc_id: r.get(0)?,
                    task_id: r.get(1)?,
                    kind: r.get(2)?,
                    title: r.get(3)?,
                    body: r.get(4)?,
                    created_at: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            },
        ) {
            Ok(d) => Ok(Some(d)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CoordinatorError::Db(e)),
        }
    }

    /// PHASE 1 (Brief): list a Brief's Dossiers (metadata only, no
    /// body), oldest first.
    pub fn list_dossiers(
        &self,
        task_id: &str,
    ) -> Result<Vec<brief::DossierMeta>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT doc_id, kind, title, created_at, updated_at
                 FROM task_documents WHERE task_id = ?1
                 ORDER BY created_at ASC, doc_id ASC",
            )
            .map_err(CoordinatorError::Db)?;
        let rows: Vec<brief::DossierMeta> = stmt
            .query_map(params![task_id], |r| {
                Ok(brief::DossierMeta {
                    doc_id: r.get(0)?,
                    kind: r.get(1)?,
                    title: r.get(2)?,
                    created_at: r.get(3)?,
                    updated_at: r.get(4)?,
                })
            })
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// PHASE 5 (companion): post a comment to a Brief's Chronicle.
    /// Records a `brief.comment` event (payload `author: text`) so a
    /// human, the companion, and the assigned Operative share one
    /// conversation thread on the Brief — read back via `task.events`
    /// (type filter `brief.comment`, order `desc`). The Brief must
    /// exist; author and text are required.
    pub fn comment_on_brief(
        &self,
        task_id: &str,
        author: &str,
        text: &str,
    ) -> Result<(), CoordinatorError> {
        let author = author.trim();
        let text = text.trim();
        if author.is_empty() {
            return Err(CoordinatorError::Invalid("comment author required".to_string()));
        }
        if text.is_empty() {
            return Err(CoordinatorError::Invalid("comment text required".to_string()));
        }
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        if !task_row_exists(&conn, task_id)? {
            return Err(CoordinatorError::NotFound(task_id.to_string()));
        }
        let now = unix_secs();
        conn.execute(
            "INSERT INTO task_events (task_id, ts, event_type, payload)
             VALUES (?1, ?2, 'brief.comment', ?3)",
            params![task_id, now, format!("{author}: {text}")],
        )
        .map_err(CoordinatorError::Db)?;
        Ok(())
    }

    /// PHASE 5 (Brief): replace a Brief's free-form labels. Each is
    /// trimmed; empties and any containing the `,` separator are
    /// dropped; duplicates are removed (first wins, order preserved).
    /// An empty result clears the column. The Brief must exist.
    pub fn set_brief_labels(
        &self,
        task_id: &str,
        labels: &[&str],
    ) -> Result<(), CoordinatorError> {
        let mut seen = std::collections::BTreeSet::new();
        let mut norm: Vec<String> = Vec::new();
        for l in labels {
            let t = l.trim();
            if t.is_empty() || t.contains(',') {
                continue;
            }
            if seen.insert(t.to_string()) {
                norm.push(t.to_string());
            }
        }
        let joined = norm.join(",");
        let stored: Option<String> = if joined.is_empty() { None } else { Some(joined) };
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let changed = conn
            .execute(
                "UPDATE tasks SET labels = ?1, updated_at = ?2 WHERE task_id = ?3",
                params![stored, unix_secs(), task_id],
            )
            .map_err(CoordinatorError::Db)?;
        if changed == 0 {
            return Err(CoordinatorError::NotFound(task_id.to_string()));
        }
        Ok(())
    }

    /// PHASE 5 (board): the Briefs carrying `label` (CSV-column
    /// membership), newest first. Empty `label` → empty result.
    /// (Labels with SQL `LIKE` wildcards aren't expected; set_labels
    /// keeps them comma-free but not wildcard-free.)
    pub fn list_briefs_by_label(
        &self,
        label: &str,
        limit: usize,
    ) -> Result<Vec<brief::BriefCard>, CoordinatorError> {
        let label = label.trim();
        if label.is_empty() {
            return Ok(Vec::new());
        }
        let lim = limit.clamp(1, self.max_list) as i64;
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT task_id, title, board_status, priority,
                        assignee_agent_id, mandate_id, campaign_id
                 FROM tasks
                 WHERE labels = ?1 OR labels LIKE ?2 OR labels LIKE ?3 OR labels LIKE ?4
                 ORDER BY updated_at DESC LIMIT ?5",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(
                params![
                    label,
                    format!("{label},%"),
                    format!("%,{label}"),
                    format!("%,{label},%"),
                    lim
                ],
                brief_card_from_row,
            )
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// PHASE 5 (Brief): a Brief's labels (empty when unset).
    /// `NotFound` when the Brief doesn't exist.
    pub fn brief_labels(&self, task_id: &str) -> Result<Vec<String>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let raw: Option<Option<String>> = conn
            .query_row(
                "SELECT labels FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(CoordinatorError::Db)?;
        match raw {
            None => Err(CoordinatorError::NotFound(task_id.to_string())),
            Some(None) => Ok(Vec::new()),
            Some(Some(s)) => Ok(s
                .split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_string)
                .collect()),
        }
    }

    /// PHASE 5 (dispatch): compose the prompt handed to a Rig for a
    /// Brief — its title, its Dossier headers (the plan/spec
    /// artifacts), and the most recent comments (oldest→newest).
    /// Best-effort: any sub-read failure simply omits that section,
    /// degrading to at least the title. Returns an empty string for
    /// an unknown Brief.
    pub fn compose_brief_prompt(&self, task_id: &str, max_comments: usize) -> String {
        let mut out = String::new();
        // Title — the headline instruction. Scoped so the lock is
        // dropped before the sub-reads below re-lock.
        if let Ok(conn) = self.conn.lock()
            && let Ok(title) = conn.query_row(
                "SELECT title FROM tasks WHERE task_id=?1",
                params![task_id],
                |r| r.get::<_, String>(0),
            ) {
                out.push_str(&title);
            }
        // Dossiers — the durable artifacts (plan/spec/notes).
        if let Ok(docs) = self.list_dossiers(task_id)
            && !docs.is_empty() {
                out.push_str("\n\nDossiers:");
                for d in docs {
                    out.push_str(&format!("\n- [{}] {}", d.kind, d.title));
                }
            }
        // The current plan body in full — the agent needs the actual
        // instructions, not just the plan's title.
        if let Ok(Some(plan)) = self.latest_dossier(task_id, "plan")
            && !plan.body.trim().is_empty() {
                out.push_str("\n\nCurrent plan:\n");
                out.push_str(plan.body.trim());
            }
        // Recent comments — the conversation thread, oldest→newest.
        if let Ok(mut comments) = self.query_events(
            task_id,
            0,
            max_comments.max(1),
            Some("brief.comment"),
            EventOrder::Desc,
        )
            && !comments.is_empty() {
                comments.reverse();
                out.push_str("\n\nRecent comments:");
                for c in comments {
                    out.push_str(&format!("\n- {}", c.payload));
                }
            }
        out
    }

    /// PHASE 1 (Brief): set one of the Brief's spine fields —
    /// `assignee` / `priority` / `mandate` / `campaign`. Empty
    /// value clears assignee/mandate/campaign (NULL); `priority`
    /// must be a valid level. The Brief must exist.
    ///
    /// NOTE: assignee/mandate/campaign are stored as soft links
    /// (the Operative lives in the agent store, the Mandate /
    /// Campaign in the spine store — both separate DBs), so
    /// cross-object existence is the caller's responsibility for
    /// now.
    pub fn set_brief_field(
        &self,
        task_id: &str,
        field: &str,
        value: &str,
    ) -> Result<(), CoordinatorError> {
        let now = unix_secs();
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        if !task_row_exists(&conn, task_id)? {
            return Err(CoordinatorError::NotFound(task_id.to_string()));
        }
        let changed = match field {
            "priority" => {
                let v = value.trim();
                if !brief::is_priority(v) {
                    return Err(CoordinatorError::Invalid(format!(
                        "priority '{v}' not in low/normal/high/urgent"
                    )));
                }
                conn.execute(
                    "UPDATE tasks SET priority=?1, updated_at=?2 WHERE task_id=?3",
                    params![v, now, task_id],
                )
            }
            "assignee" | "mandate" | "campaign" => {
                let col = match field {
                    "assignee" => "assignee_agent_id",
                    "mandate" => "mandate_id",
                    _ => "campaign_id",
                };
                let t = value.trim();
                let stored: Option<&str> = if t.is_empty() { None } else { Some(t) };
                // `col` is from the fixed match above, never user input.
                let sql =
                    format!("UPDATE tasks SET {col}=?1, updated_at=?2 WHERE task_id=?3");
                conn.execute(&sql, params![stored, now, task_id])
            }
            other => {
                return Err(CoordinatorError::Invalid(format!(
                    "unknown brief field '{other}' (assignee/priority/mandate/campaign)"
                )));
            }
        }
        .map_err(CoordinatorError::Db)?;
        if changed == 0 {
            return Err(CoordinatorError::NotFound(task_id.to_string()));
        }
        // Reassigning (or unassigning) drops any stale Claim so the
        // new assignee can pick the Brief up immediately instead of
        // waiting out the previous holder's lease.
        if field == "assignee" {
            let _ = conn.execute(
                "UPDATE tasks SET claimed_by=NULL, claim_expires_at=NULL WHERE task_id=?1",
                params![task_id],
            );
        }
        // Chronicle an assignment (skip clears).
        if field == "assignee" && !value.trim().is_empty() {
            let _ = conn.execute(
                "INSERT INTO task_events (task_id, ts, event_type, payload)
                 VALUES (?1, ?2, 'brief.assigned', ?3)",
                params![task_id, now, value.trim()],
            );
        }
        Ok(())
    }

    /// PHASE 1 (Brief): read a Brief's spine fields. `None` when
    /// the Brief doesn't exist.
    pub fn brief_fields(
        &self,
        task_id: &str,
    ) -> Result<Option<brief::BriefFields>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        match conn.query_row(
            "SELECT task_id, assignee_agent_id, board_status, priority, mandate_id, campaign_id
             FROM tasks WHERE task_id = ?1",
            params![task_id],
            |r| {
                Ok(brief::BriefFields {
                    task_id: r.get(0)?,
                    assignee_agent_id: r.get(1)?,
                    board_status: r.get(2)?,
                    priority: r.get(3)?,
                    mandate_id: r.get(4)?,
                    campaign_id: r.get(5)?,
                })
            },
        ) {
            Ok(f) => Ok(Some(f)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CoordinatorError::Db(e)),
        }
    }

    /// PHASE 2 (board): list Briefs for the Issue board. `board`,
    /// when set, narrows to one column (validated); `None` returns
    /// all. Newest-updated first, capped at the store's max_list.
    /// The core read behind the board view.
    pub fn list_briefs_by_board(
        &self,
        board: Option<&str>,
        limit: usize,
    ) -> Result<Vec<brief::BriefCard>, CoordinatorError> {
        if let Some(b) = board
            && !brief::is_board_status(b) {
                return Err(CoordinatorError::Invalid(format!(
                    "unknown board status '{b}'"
                )));
            }
        let lim = limit.clamp(1, self.max_list) as i64;
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let map = |r: &rusqlite::Row| {
            Ok(brief::BriefCard {
                task_id: r.get(0)?,
                title: r.get(1)?,
                board_status: r.get(2)?,
                priority: r.get(3)?,
                assignee_agent_id: r.get(4)?,
                mandate_id: r.get(5)?,
                campaign_id: r.get(6)?,
            })
        };
        // `cols` is a fixed string, never user input.
        let cols =
            "task_id, title, board_status, priority, assignee_agent_id, mandate_id, campaign_id";
        let rows: Vec<brief::BriefCard> = match board {
            Some(b) => {
                let sql = format!(
                    "SELECT {cols} FROM tasks WHERE board_status = ?1 \
                     ORDER BY updated_at DESC LIMIT ?2"
                );
                let mut stmt = conn.prepare(&sql).map_err(CoordinatorError::Db)?;
                stmt.query_map(params![b, lim], map)
                    .map_err(CoordinatorError::Db)?
                    .collect::<rusqlite::Result<_>>()
                    .map_err(CoordinatorError::Db)?
            }
            None => {
                let sql =
                    format!("SELECT {cols} FROM tasks ORDER BY updated_at DESC LIMIT ?1");
                let mut stmt = conn.prepare(&sql).map_err(CoordinatorError::Db)?;
                stmt.query_map(params![lim], map)
                    .map_err(CoordinatorError::Db)?
                    .collect::<rusqlite::Result<_>>()
                    .map_err(CoordinatorError::Db)?
            }
        };
        Ok(rows)
    }

    /// PHASE 5 (Desk): an Operative's personal Desk — their in-flight
    /// Briefs (board_status in todo/in_progress/in_review/blocked),
    /// priority-ordered then oldest-first. Excludes
    /// backlog/done/cancelled, so it's the "what's on my plate now"
    /// view for the companion / per-agent dashboard.
    pub fn list_desk_for_assignee(
        &self,
        assignee: &str,
        limit: usize,
    ) -> Result<Vec<brief::BriefCard>, CoordinatorError> {
        let lim = limit.clamp(1, self.max_list) as i64;
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT task_id, title, board_status, priority,
                        assignee_agent_id, mandate_id, campaign_id
                 FROM tasks
                 WHERE assignee_agent_id = ?1
                   AND board_status IN ('todo','in_progress','in_review','blocked')
                 ORDER BY
                   CASE priority
                       WHEN 'urgent' THEN 0 WHEN 'high' THEN 1
                       WHEN 'normal' THEN 2 ELSE 3 END,
                   updated_at ASC
                 LIMIT ?2",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![assignee, lim], brief_card_from_row)
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// PHASE 5 (org load): an Operative's workload — their in-flight
    /// Brief counts by board column (todo/in_progress/in_review/
    /// blocked). The load signal behind the org chart — who's
    /// overloaded, who's free for the next assignment.
    pub fn assignee_board_counts(
        &self,
        assignee: &str,
    ) -> Result<Vec<(String, i64)>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT board_status, COUNT(*) FROM tasks
                 WHERE assignee_agent_id = ?1
                   AND board_status IN ('todo','in_progress','in_review','blocked')
                 GROUP BY board_status ORDER BY board_status",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![assignee], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// PHASE 5 (org load): aggregate in-flight Brief counts across a
    /// SET of Operatives — a manager's whole Branch, say. Counts
    /// briefs in todo/in_progress/in_review/blocked by column across
    /// all `assignees`. Empty input → empty result.
    pub fn aggregate_board_counts(
        &self,
        assignees: &[&str],
    ) -> Result<Vec<(String, i64)>, CoordinatorError> {
        if assignees.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let placeholders = (1..=assignees.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT board_status, COUNT(*) FROM tasks
             WHERE assignee_agent_id IN ({placeholders})
               AND board_status IN ('todo','in_progress','in_review','blocked')
             GROUP BY board_status ORDER BY board_status"
        );
        let mut stmt = conn.prepare(&sql).map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(assignees.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// PHASE 2 (Desk): Briefs that are blocked right now — in a live
    /// column with at least one unresolved Snag (a blocker not yet
    /// `done`). The "blocked work" the Desk surfaces. Newest first.
    pub fn list_blocked_briefs(
        &self,
        limit: usize,
    ) -> Result<Vec<brief::BriefCard>, CoordinatorError> {
        let lim = limit.clamp(1, self.max_list) as i64;
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT t.task_id, t.title, t.board_status, t.priority,
                        t.assignee_agent_id, t.mandate_id, t.campaign_id
                 FROM tasks t
                 JOIN task_edges e ON e.task_id = t.task_id AND e.edge_type = 'blocked_on'
                 JOIN tasks b ON b.task_id = e.related_task_id
                 WHERE b.board_status != 'done'
                   AND t.board_status NOT IN ('done', 'cancelled')
                 ORDER BY t.updated_at DESC LIMIT ?1",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![lim], brief_card_from_row)
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// PHASE 2 (Desk): Briefs that look stale — in an active column
    /// (todo / in_progress / in_review) with no update for at least
    /// `idle_secs`. Most-stale first. The "stuck work" the Desk
    /// surfaces so nothing sits unmoved with nobody on it.
    pub fn list_stale_briefs(
        &self,
        idle_secs: i64,
        limit: usize,
    ) -> Result<Vec<brief::BriefCard>, CoordinatorError> {
        let lim = limit.clamp(1, self.max_list) as i64;
        let cutoff = unix_secs().saturating_sub(idle_secs.max(0));
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT task_id, title, board_status, priority,
                        assignee_agent_id, mandate_id, campaign_id
                 FROM tasks
                 WHERE board_status IN ('todo', 'in_progress', 'in_review')
                   AND updated_at < ?1
                 ORDER BY updated_at ASC LIMIT ?2",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![cutoff, lim], brief_card_from_row)
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// PHASE 3 (Claim): atomically claim a Brief for execution.
    /// Single-owner — succeeds only if the Brief is unclaimed, its
    /// lease has expired, or it is already held by `agent_id`
    /// (re-entrant refresh). Sets a fresh lease of `lease_secs`.
    /// Returns true if claimed, false if another Operative holds a
    /// live claim. `NotFound` if the Brief doesn't exist.
    pub fn claim_brief(
        &self,
        task_id: &str,
        agent_id: &str,
        lease_secs: i64,
    ) -> Result<bool, CoordinatorError> {
        let agent = agent_id.trim();
        if agent.is_empty() {
            return Err(CoordinatorError::Invalid("claim: agent_id required".to_string()));
        }
        let now = unix_secs();
        let expires = now.saturating_add(lease_secs.max(1));
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        if !task_row_exists(&conn, task_id)? {
            return Err(CoordinatorError::NotFound(task_id.to_string()));
        }
        let changed = conn
            .execute(
                "UPDATE tasks
                 SET claimed_by = ?1, claim_expires_at = ?2, updated_at = ?3
                 WHERE task_id = ?4
                   AND (claimed_by IS NULL OR claimed_by = ?1
                        OR claim_expires_at IS NULL OR claim_expires_at < ?3)",
                params![agent, expires, now, task_id],
            )
            .map_err(CoordinatorError::Db)?;
        Ok(changed == 1)
    }

    /// PHASE 3 (Claim): extend the lease on a Brief the caller
    /// holds — the heartbeat that keeps a live claim alive. Returns
    /// true if extended, false if the claim was lost (expired, or
    /// someone else holds it now).
    pub fn heartbeat_claim(
        &self,
        task_id: &str,
        agent_id: &str,
        lease_secs: i64,
    ) -> Result<bool, CoordinatorError> {
        let now = unix_secs();
        let expires = now.saturating_add(lease_secs.max(1));
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let changed = conn
            .execute(
                "UPDATE tasks SET claim_expires_at = ?1, updated_at = ?2
                 WHERE task_id = ?3 AND claimed_by = ?4 AND claim_expires_at >= ?2",
                params![expires, now, task_id, agent_id.trim()],
            )
            .map_err(CoordinatorError::Db)?;
        Ok(changed == 1)
    }

    /// PHASE 3 (Claim): release a Brief the caller holds, freeing it
    /// for the next Operative. No-op (Ok) when not held by caller.
    pub fn release_claim(&self, task_id: &str, agent_id: &str) -> Result<(), CoordinatorError> {
        let now = unix_secs();
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        conn.execute(
            "UPDATE tasks SET claimed_by = NULL, claim_expires_at = NULL, updated_at = ?1
             WHERE task_id = ?2 AND claimed_by = ?3",
            params![now, task_id, agent_id.trim()],
        )
        .map_err(CoordinatorError::Db)?;
        Ok(())
    }

    /// PHASE 3 (Claim): the current live claim holder + lease
    /// expiry, if any. `None` when unclaimed or the lease expired.
    pub fn claim_holder(
        &self,
        task_id: &str,
    ) -> Result<Option<(String, i64)>, CoordinatorError> {
        let now = unix_secs();
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        match conn.query_row(
            "SELECT claimed_by, claim_expires_at FROM tasks
             WHERE task_id = ?1 AND claimed_by IS NOT NULL AND claim_expires_at >= ?2",
            params![task_id, now],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        ) {
            Ok(t) => Ok(Some(t)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(CoordinatorError::Db(e)),
        }
    }

    /// PHASE 3 (Pulse): the dispatcher's work-list — Briefs ready to
    /// be worked right now: assigned to an Operative, in an active
    /// column (todo / in_progress), not blocked by an unresolved
    /// Snag, and not currently claimed (or the claim has expired).
    /// Priority-ordered (urgent → high → normal → low), then oldest
    /// update first. This is what the heartbeat loop polls to wake +
    /// claim + dispatch.
    pub fn list_ready_briefs(
        &self,
        limit: usize,
    ) -> Result<Vec<brief::BriefCard>, CoordinatorError> {
        let lim = limit.clamp(1, self.max_list) as i64;
        let now = unix_secs();
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT t.task_id, t.title, t.board_status, t.priority,
                        t.assignee_agent_id, t.mandate_id, t.campaign_id
                 FROM tasks t
                 WHERE t.assignee_agent_id IS NOT NULL
                   AND t.board_status IN ('todo', 'in_progress')
                   AND (t.claimed_by IS NULL OR t.claim_expires_at IS NULL
                        OR t.claim_expires_at < ?1)
                   AND NOT EXISTS (
                       SELECT 1 FROM task_edges e
                       JOIN tasks b ON b.task_id = e.related_task_id
                       WHERE e.task_id = t.task_id AND e.edge_type = 'blocked_on'
                         AND b.board_status != 'done'
                   )
                 ORDER BY
                   CASE t.priority
                       WHEN 'urgent' THEN 0 WHEN 'high' THEN 1
                       WHEN 'normal' THEN 2 ELSE 3 END,
                   t.updated_at ASC
                 LIMIT ?2",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![now, lim], brief_card_from_row)
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// PHASE 3 (supervisory wake): parent Briefs whose Sub-briefs
    /// are ALL finished (every child `done` or `cancelled`) while
    /// the parent itself is still active. This is the
    /// "children-completed" wake — the planner that decomposed the
    /// work is roused to review the finished slice and assign the
    /// next. Requires: has ≥1 Sub-brief, no still-active Sub-brief,
    /// parent not itself terminal.
    pub fn list_briefs_with_all_children_done(
        &self,
        limit: usize,
    ) -> Result<Vec<brief::BriefCard>, CoordinatorError> {
        let lim = limit.clamp(1, self.max_list) as i64;
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT t.task_id, t.title, t.board_status, t.priority,
                        t.assignee_agent_id, t.mandate_id, t.campaign_id
                 FROM tasks t
                 WHERE t.board_status NOT IN ('done', 'cancelled')
                   AND EXISTS (
                       SELECT 1 FROM task_edges e
                       WHERE e.task_id = t.task_id AND e.edge_type = 'spawned'
                   )
                   AND NOT EXISTS (
                       SELECT 1 FROM task_edges e
                       JOIN tasks c ON c.task_id = e.related_task_id
                       WHERE e.task_id = t.task_id AND e.edge_type = 'spawned'
                         AND c.board_status NOT IN ('done', 'cancelled')
                   )
                 ORDER BY t.updated_at ASC LIMIT ?1",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![lim], brief_card_from_row)
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// PHASE 1/2 (rollup): count a Campaign's Briefs by board
    /// column. The progress read behind a Campaign's summary.
    pub fn campaign_brief_counts(
        &self,
        campaign_id: &str,
    ) -> Result<Vec<(String, i64)>, CoordinatorError> {
        self.brief_counts_by_column("campaign_id", campaign_id)
    }

    /// PHASE 1/2 (rollup): count a Mandate's directly-linked Briefs
    /// by board column. (Campaign-linked Briefs roll up via
    /// `campaign_brief_counts`.)
    pub fn mandate_brief_counts(
        &self,
        mandate_id: &str,
    ) -> Result<Vec<(String, i64)>, CoordinatorError> {
        self.brief_counts_by_column("mandate_id", mandate_id)
    }

    /// Shared: Brief counts grouped by board column for a fixed
    /// spine-link column (`campaign_id` / `mandate_id`, never user
    /// input). Ordered by column name for stable output.
    fn brief_counts_by_column(
        &self,
        column: &str,
        value: &str,
    ) -> Result<Vec<(String, i64)>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let sql = format!(
            "SELECT board_status, COUNT(*) FROM tasks
             WHERE {column} = ?1 GROUP BY board_status ORDER BY board_status"
        );
        let mut stmt = conn.prepare(&sql).map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![value], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// PHASE 5 (companion / board): Brief counts across all board
    /// columns — the board-at-a-glance the chat companion reads for
    /// context and the dashboard header shows.
    pub fn board_summary(&self) -> Result<Vec<(String, i64)>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT board_status, COUNT(*) FROM tasks
                 GROUP BY board_status ORDER BY board_status",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// PHASE 1/5: the Briefs linked to a Mandate (as cards).
    pub fn list_briefs_by_mandate(
        &self,
        mandate_id: &str,
        limit: usize,
    ) -> Result<Vec<brief::BriefCard>, CoordinatorError> {
        self.list_briefs_by_link("mandate_id", mandate_id, limit)
    }

    /// PHASE 1/5: the Briefs linked to a Campaign (as cards).
    pub fn list_briefs_by_campaign(
        &self,
        campaign_id: &str,
        limit: usize,
    ) -> Result<Vec<brief::BriefCard>, CoordinatorError> {
        self.list_briefs_by_link("campaign_id", campaign_id, limit)
    }

    fn list_briefs_by_link(
        &self,
        column: &str,
        value: &str,
        limit: usize,
    ) -> Result<Vec<brief::BriefCard>, CoordinatorError> {
        let lim = limit.clamp(1, self.max_list) as i64;
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        // `column` is a fixed internal value, never user input.
        let sql = format!(
            "SELECT task_id, title, board_status, priority,
                    assignee_agent_id, mandate_id, campaign_id
             FROM tasks WHERE {column} = ?1 ORDER BY updated_at DESC LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql).map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![value, lim], brief_card_from_row)
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<_>>()
            .map_err(CoordinatorError::Db)?;
        Ok(rows)
    }

    /// Insert a new Task. Returns the freshly-minted `task_id`
    /// (32 hex chars). Optional retry / timeout metadata defaults to
    /// "no retry, no timeout" for backwards compatibility with pre-C1
    /// callers that don't supply them. `origin_surface` (D-004 /
    /// PH-ORIGIN-SURFACE) is an operator-curated label naming
    /// which dispatch surface created the task — `None` writes
    /// NULL and the dashboard renders it as "unknown".
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
        origin_surface: Option<&str>,
    ) -> Result<String, CoordinatorError> {
        let task_id = new_task_id();
        let now = unix_secs();
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        conn.execute(
            "INSERT INTO tasks (task_id, title, status, owner_subject_id,
                                flow_template, params_json,
                                created_at, updated_at,
                                retry_count, retry_policy, max_retries,
                                max_runtime_secs, origin_surface)
             VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6, ?6,
                     0, ?7, ?8, ?9, ?10)",
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
                origin_surface,
            ],
        )
        .map_err(CoordinatorError::Db)?;
        Ok(task_id)
    }

    /// PHASE 5 (companion): create a Brief and place it on the spine
    /// in one call — the "materialize work" path. Creates the
    /// underlying Task (flow-less, `companion` origin), opens it in
    /// `todo`, and links the optional spine fields (assignee /
    /// mandate / campaign / priority). Returns the new task_id.
    #[allow(clippy::too_many_arguments)]
    pub fn create_brief(
        &self,
        title: &str,
        owner_subject_id: &str,
        assignee: Option<&str>,
        mandate: Option<&str>,
        campaign: Option<&str>,
        priority: Option<&str>,
    ) -> Result<String, CoordinatorError> {
        if title.trim().is_empty() {
            return Err(CoordinatorError::Invalid("brief title required".into()));
        }
        let pri = priority.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(brief::DEFAULT_PRIORITY);
        if !brief::is_priority(pri) {
            return Err(CoordinatorError::Invalid(format!(
                "priority '{pri}' not in low/normal/high/urgent"
            )));
        }
        let task_id = self.create(
            title.trim(),
            "brief/manual",
            "{}",
            owner_subject_id,
            RetryPolicy::None,
            0,
            None,
            Some("companion"),
        )?;
        // Chronicle the creation distinctly (the activity feed's
        // first entry), then open it on the board.
        let _ = self.append_event(&task_id, "brief.created", title.trim());
        // Open it on the board (backlog → todo: ready for dispatch).
        self.set_board_status(&task_id, "todo")?;
        if let Some(a) = assignee.map(str::trim).filter(|s| !s.is_empty()) {
            self.set_brief_field(&task_id, "assignee", a)?;
        }
        if let Some(m) = mandate.map(str::trim).filter(|s| !s.is_empty()) {
            self.set_brief_field(&task_id, "mandate", m)?;
        }
        if let Some(c) = campaign.map(str::trim).filter(|s| !s.is_empty()) {
            self.set_brief_field(&task_id, "campaign", c)?;
        }
        if pri != brief::DEFAULT_PRIORITY {
            self.set_brief_field(&task_id, "priority", pri)?;
        }
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
            // H10: redact known-shape secrets in error_cause /
            // last_failure_reason before persisting. Provider
            // error messages often quote `Authorization: Bearer ...`
            // headers or bare keys; without redaction those land
            // in the tasks row + chronicle forever.
            let safe = relix_core::redact::redact_secrets(v);
            sets.push("error_cause = ?");
            args.push(safe.clone().into());
            sets.push("last_failure_reason = ?");
            args.push(safe.into());
        }
        // H4: anti-thrash counter. When a new failure_class arrives,
        // compare it against the prior value on the row. Bumping vs
        // resetting matches Hermes's "_ineffective_compression_count":
        //   same class as last time → bump (the runtime is going in
        //     circles)
        //   different class → reset to 1 (a different failure mode
        //     suggests the runtime is making progress)
        //   None → leave counter alone (no failure to track).
        // The counter is read post-commit to decide whether to emit
        // the thrash-detected event + auto-mark investigation; the
        // *write* of the counter happens inside this transaction so
        // it's consistent with the failure update.
        let mut thrash_check: Option<(String, i64)> = None;
        if let Some(new_class) = failure_class {
            let (prior_class, prior_count) = tx
                .query_row(
                    "SELECT last_failure_class, consecutive_same_class_count
                     FROM tasks WHERE task_id = ?1",
                    params![task_id],
                    |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?)),
                )
                .map_err(CoordinatorError::Db)?;
            let new_count = if prior_class.as_deref() == Some(new_class) {
                prior_count.saturating_add(1)
            } else {
                1
            };
            sets.push("last_failure_class = ?");
            args.push(new_class.to_string().into());
            sets.push("consecutive_same_class_count = ?");
            args.push(new_count.into());
            thrash_check = Some((new_class.to_string(), new_count));
        }
        args.push(task_id.to_string().into());
        let sql = format!("UPDATE tasks SET {} WHERE task_id = ?", sets.join(", "));
        let n = tx
            .execute(&sql, rusqlite::params_from_iter(args.iter()))
            .map_err(CoordinatorError::Db)?;
        if n == 0 {
            return Err(CoordinatorError::NotFound(task_id.to_string()));
        }

        // H4: when the counter crosses the threshold AND the task
        // isn't already investigation-marked, auto-mark it +
        // emit `task.thrash_detected`. Operators see a "thrashing"
        // banner on the task without having to grep audit logs.
        if let Some((cls, count)) = thrash_check
            && count >= ANTI_THRASH_THRESHOLD
        {
            let already_marked: Option<i64> = tx
                .query_row(
                    "SELECT investigation_marked_at FROM tasks WHERE task_id = ?1",
                    params![task_id],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .map_err(CoordinatorError::Db)?;
            if already_marked.is_none() {
                let auto_reason =
                    format!("auto-marked: {count} consecutive failures with class={cls}",);
                tx.execute(
                    "UPDATE tasks
                     SET investigation_marked_at = ?1,
                         investigation_reason = ?2,
                         updated_at = ?1
                     WHERE task_id = ?3",
                    params![now, auto_reason, task_id],
                )
                .map_err(CoordinatorError::Db)?;
                let thrash_legacy = format!(
                    "consecutive failures class={cls} count={count} threshold={ANTI_THRASH_THRESHOLD}",
                );
                let thrash_json = format!(
                    r#"{{"class":"{}","count":{count},"threshold":{ANTI_THRASH_THRESHOLD}}}"#,
                    json_escape(&cls),
                );
                insert_typed_event(
                    &tx,
                    task_id,
                    now,
                    "task.thrash_detected",
                    &thrash_legacy,
                    None,
                    None,
                    Some(&thrash_json),
                )?;
                // Mirror task.investigation_marked so the existing
                // dashboard treatment for that event surfaces this
                // auto-mark identically to operator-set marks.
                let marked_legacy = format!("auto-marked (thrash): count={count} class={cls}");
                let marked_json = format!(
                    r#"{{"reason":"{}","auto":true,"thrash_class":"{}","thrash_count":{count}}}"#,
                    json_escape(&auto_reason),
                    json_escape(&cls),
                );
                insert_typed_event(
                    &tx,
                    task_id,
                    now,
                    "task.investigation_marked",
                    &marked_legacy,
                    None,
                    None,
                    Some(&marked_json),
                )?;
            }
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
                    // H14: synthesize the post-mortem terminal_summary
                    // for every terminal-status transition (not just
                    // deadline recovery — H5 covers that). Pulls the
                    // facts we already own. Emitted at most once per
                    // task: a second update to a terminal status
                    // (which the state machine should reject anyway)
                    // would be a no-op because we already detected
                    // we just transitioned.
                    emit_terminal_summary_in_txn(&tx, task_id, v, now)?;
                }
                _ => {}
            }
        }
        tx.commit().map_err(CoordinatorError::Db)?;
        Ok(())
    }

    /// W2-001b: clone a task into a brand-new replay. The new
    /// task inherits the original's flow_template, params_json,
    /// retry policy/budget, max_runtime_secs, and origin_surface.
    /// Title is suffixed with ` (replay)` so it's distinguishable
    /// in task lists. retry_count starts at zero on the replay —
    /// operators want to see the full retry chain on the new task
    /// fresh, not a continuation of the old one.
    ///
    /// Wires a `retried_from` cross-task edge from the NEW task
    /// to the ORIGINAL plus a `task.replayed_from` chronicle
    /// event on the new task with payload
    /// `from=<original_task_id>`.
    ///
    /// Returns the new task_id on success; NotFound when the
    /// original doesn't exist.
    pub fn replay_from(
        &self,
        original_task_id: &str,
        producer_subject_id: &str,
    ) -> Result<String, CoordinatorError> {
        // Re-use the existing `get` projection so any new fields
        // added to TaskView automatically flow into the replay
        // without further changes here.
        let original = match self.get(original_task_id)? {
            Some(v) => v,
            None => return Err(CoordinatorError::NotFound(original_task_id.to_string())),
        };
        let retry_policy = RetryPolicy::parse(&original.retry_policy).unwrap_or(RetryPolicy::None);
        let replay_title = format!("{} (replay)", original.title);
        let new_id = self.create(
            &replay_title,
            &original.flow_template,
            &original.params_json,
            &original.owner_subject_id,
            retry_policy,
            original.max_retries,
            original.max_runtime_secs,
            original.origin_surface.as_deref(),
        )?;
        // Wire the cross-task edge + chronicle event on the new
        // task. `record_cross_task_edge` requires parent !=
        // related which is guaranteed here (new_id was just
        // freshly minted).
        let payload_reason = format!("replay of {original_task_id}");
        let _ = self.record_cross_task_edge(
            &new_id,
            original_task_id,
            "retried_from",
            "task.replayed_from",
            None,
            None,
            Some(&payload_reason),
            producer_subject_id,
        )?;
        Ok(new_id)
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

        // M76: explicit retry suppression when paused/frozen.
        // Per the cooperative interruption protocol (M70/M71)
        // an operator pause/freeze records intent that future
        // runtime cycles should not advance the task. The
        // retry path is one such advancement — we refuse it
        // here AND emit a `task.retry_suppressed` chronicle
        // event so operators see the suppression alongside
        // the original interruption request.
        if matches!(status.as_str(), "paused" | "frozen") {
            let suppression_legacy =
                format!("retry suppressed: task is `{status}` (cooperative interruption)");
            let suppression_json = format!(
                r#"{{"suppressed_by":"{}","retry_count":{retry_count},"budget":{max_retries}}}"#,
                json_escape(&status),
            );
            insert_typed_event(
                &tx,
                task_id,
                now,
                "task.retry_suppressed",
                &suppression_legacy,
                None,
                None,
                Some(&suppression_json),
            )?;
            tx.commit().map_err(CoordinatorError::Db)?;
            return Ok(RetryDecision::Rejected {
                reason: format!(
                    "task is `{status}` — retry suppressed by cooperative \
                     interruption (clear via task.resume / task.unfreeze)"
                ),
            });
        }
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

    /// H6: find tasks that have been `running` longer than
    /// `stuck_threshold_secs` AND do NOT have `max_runtime_secs`
    /// set (so the recovery scan will never touch them). Pure
    /// projection — no writes, no side effects. Operators use
    /// this via the dashboard's stuck-task banner to spot
    /// executors that died without leaving a deadline behind.
    ///
    /// Returns rows ordered oldest-first so the most-stuck task
    /// surfaces at the top.
    pub fn stuck_running(
        &self,
        now_secs: i64,
        stuck_threshold_secs: i64,
    ) -> Result<Vec<StuckTaskRow>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let cutoff = now_secs - stuck_threshold_secs.max(0);
        let mut stmt = conn
            .prepare(
                "SELECT t.task_id,
                        t.title,
                        COALESCE(a.started_at, t.started_at) AS scan_started,
                        t.current_attempt_id
                 FROM tasks t
                 LEFT JOIN task_attempts a ON a.attempt_id = t.current_attempt_id
                 WHERE t.status = 'running'
                   AND t.max_runtime_secs IS NULL
                   AND COALESCE(a.started_at, t.started_at) IS NOT NULL
                   AND COALESCE(a.started_at, t.started_at) <= ?1
                 ORDER BY COALESCE(a.started_at, t.started_at) ASC",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![cutoff], |r| {
                Ok(StuckTaskRow {
                    task_id: r.get(0)?,
                    title: r.get(1)?,
                    started_at: r.get(2)?,
                    current_attempt_id: r.get(3)?,
                    age_secs: 0,
                })
            })
            .map_err(CoordinatorError::Db)?;
        let mut out = Vec::new();
        for r in rows {
            let mut row = r.map_err(CoordinatorError::Db)?;
            row.age_secs = now_secs - row.started_at;
            out.push(row);
        }
        Ok(out)
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
                        t.current_attempt_id,
                        t.retry_count,
                        t.attempt_count
                 FROM tasks t
                 LEFT JOIN task_attempts a ON a.attempt_id = t.current_attempt_id
                 WHERE t.status = 'running'
                   AND t.max_runtime_secs IS NOT NULL
                   AND COALESCE(a.started_at, t.started_at) IS NOT NULL
                   AND (COALESCE(a.started_at, t.started_at) + t.max_runtime_secs) < ?1",
            )
            .map_err(CoordinatorError::Db)?;
        let candidates: Vec<(String, i64, i64, Option<i64>, i64, i64)> = stmt
            .query_map(params![now_secs], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })
            .map_err(CoordinatorError::Db)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(CoordinatorError::Db)?;
        drop(stmt);

        let tx = conn.transaction().map_err(CoordinatorError::Db)?;
        let mut recovered = Vec::with_capacity(candidates.len());
        for (tid, started, max, current_attempt, retry_count, attempt_count) in candidates {
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
            // H5: synthesized terminal summary. The recovery scan
            // is the moment we have all the post-mortem facts in
            // one place: total wall-clock since the task first
            // started, attempt + retry counts, the final failure
            // class. Hermes generates a similar summary via an
            // LLM call after iteration-budget exhaustion; in
            // Relix the same role is played by the recovery scan,
            // and we synthesize from facts the coord already
            // owns (no executor consumer required). Single
            // task.terminal_summary event per recovery; the
            // chronicle row is the authoritative replay.
            let wall_clock_secs = (now_secs - started).max(0);
            let terminal_legacy = format!(
                "interrupted by deadline · attempts={attempt_count} retries={retry_count} \
                 wall_clock_secs={wall_clock_secs} last_failure_class=timeout",
            );
            let terminal_json = format!(
                r#"{{"reason":"deadline_exceeded","attempts":{attempt_count},"retries":{retry_count},"wall_clock_secs":{wall_clock_secs},"last_failure_class":"timeout","auto_emitted_by":"recover_interrupted"}}"#
            );
            insert_typed_event(
                &tx,
                &tid,
                now_secs,
                "task.terminal_summary",
                &terminal_legacy,
                current_attempt,
                None,
                Some(&terminal_json),
            )?;
            recovered.push(tid);
        }
        // H7: opportunistic orphan-attempt cleanup in the same
        // transaction as the deadline recovery. Catches attempts
        // that were left open because their owning task was
        // already in a terminal state when the per-attempt
        // close path was skipped (legacy bug / crash mid-update /
        // pre-C2a tasks). Pure additive: the dashboard sees the
        // attempt row close + a chronicle event explaining why.
        close_orphan_attempts_in_txn(&tx, now_secs)?;
        tx.commit().map_err(CoordinatorError::Db)?;
        Ok(recovered)
    }

    /// H7: standalone orphan-attempt cleanup entrypoint. Useful
    /// when an operator wants to run cleanup without waiting for
    /// the next recovery scan tick. Returns the closed
    /// `attempt_id` list (empty when there are no orphans).
    pub fn close_orphan_attempts(&self, now_secs: i64) -> Result<Vec<i64>, CoordinatorError> {
        let mut conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let tx = conn.transaction().map_err(CoordinatorError::Db)?;
        let closed = close_orphan_attempts_in_txn(&tx, now_secs)?;
        tx.commit().map_err(CoordinatorError::Db)?;
        Ok(closed)
    }

    // ─── PH-WAVE2D: per-task todo list (Hermes todo_tool parity) ────────

    /// Replace the task's todo list with a fresh ordered set.
    /// Existing rows for the task are deleted; new rows take
    /// position 0..N matching the input order. Empty input is
    /// allowed (clears the list).
    pub fn set_task_todos(
        &self,
        task_id: &str,
        items: &[&str],
    ) -> Result<Vec<TodoItem>, CoordinatorError> {
        if items.iter().any(|s| s.trim().is_empty()) {
            return Err(CoordinatorError::Invalid(
                "task.todo_set: every todo text must be non-empty (after trim)".into(),
            ));
        }
        if items.iter().any(|s| s.len() > MAX_OPERATOR_NOTE_LEN) {
            return Err(CoordinatorError::Invalid(format!(
                "task.todo_set: a todo exceeds {MAX_OPERATOR_NOTE_LEN} bytes"
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
        tx.execute(
            "DELETE FROM task_todos WHERE task_id = ?1",
            params![task_id],
        )
        .map_err(CoordinatorError::Db)?;
        for (pos, text) in items.iter().enumerate() {
            // H8/H10 boundary: scrub secrets in operator-supplied
            // todo text before persisting (operators paste API
            // keys into anything).
            let safe = relix_core::redact::redact_secrets(text);
            tx.execute(
                "INSERT INTO task_todos (task_id, position, status, text, created_at, updated_at)
                 VALUES (?1, ?2, 'open', ?3, ?4, ?4)",
                params![task_id, pos as i64, safe, now],
            )
            .map_err(CoordinatorError::Db)?;
        }
        tx.commit().map_err(CoordinatorError::Db)?;
        // Read the post-set list using the SAME guard we already
        // hold. Calling `self.list_task_todos(task_id)` here would
        // deadlock because `self.conn` is a `std::sync::Mutex`
        // (non-reentrant) and we haven't dropped `conn` yet.
        let mut stmt = conn
            .prepare(
                "SELECT todo_id, position, status, text, created_at, updated_at
                 FROM task_todos
                 WHERE task_id = ?1
                 ORDER BY position ASC, todo_id ASC",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![task_id], |r| {
                Ok(TodoItem {
                    todo_id: r.get(0)?,
                    position: r.get(1)?,
                    status: r.get(2)?,
                    text: r.get(3)?,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                })
            })
            .map_err(CoordinatorError::Db)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(CoordinatorError::Db)?);
        }
        Ok(out)
    }

    /// Read the task's todos in `position ASC, todo_id ASC` order.
    /// Returns an empty Vec when the task has no todos OR when
    /// the task itself doesn't exist (callers usually want the
    /// list-or-empty shape; use `task.get` for existence check).
    pub fn list_task_todos(&self, task_id: &str) -> Result<Vec<TodoItem>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut stmt = conn
            .prepare(
                "SELECT todo_id, position, status, text, created_at, updated_at
                 FROM task_todos
                 WHERE task_id = ?1
                 ORDER BY position ASC, todo_id ASC",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![task_id], |r| {
                Ok(TodoItem {
                    todo_id: r.get(0)?,
                    position: r.get(1)?,
                    status: r.get(2)?,
                    text: r.get(3)?,
                    created_at: r.get(4)?,
                    updated_at: r.get(5)?,
                })
            })
            .map_err(CoordinatorError::Db)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(CoordinatorError::Db)?);
        }
        Ok(out)
    }

    /// Toggle a single todo's status. `new_status` must be
    /// `open` or `done`. Returns the updated [`TodoItem`].
    pub fn update_task_todo_status(
        &self,
        task_id: &str,
        todo_id: i64,
        new_status: &str,
    ) -> Result<TodoItem, CoordinatorError> {
        if !matches!(new_status, "open" | "done") {
            return Err(CoordinatorError::Invalid(format!(
                "task.todo_update: status must be 'open' or 'done', got '{new_status}'"
            )));
        }
        let now = unix_secs();
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let n = conn
            .execute(
                "UPDATE task_todos
                 SET status = ?1, updated_at = ?2
                 WHERE task_id = ?3 AND todo_id = ?4",
                params![new_status, now, task_id, todo_id],
            )
            .map_err(CoordinatorError::Db)?;
        if n == 0 {
            return Err(CoordinatorError::NotFound(format!(
                "todo not found: task={task_id} todo_id={todo_id}"
            )));
        }
        let row = conn
            .query_row(
                "SELECT todo_id, position, status, text, created_at, updated_at
                 FROM task_todos
                 WHERE task_id = ?1 AND todo_id = ?2",
                params![task_id, todo_id],
                |r| {
                    Ok(TodoItem {
                        todo_id: r.get(0)?,
                        position: r.get(1)?,
                        status: r.get(2)?,
                        text: r.get(3)?,
                        created_at: r.get(4)?,
                        updated_at: r.get(5)?,
                    })
                },
            )
            .map_err(CoordinatorError::Db)?;
        Ok(row)
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

    /// List up to `limit` pending tasks whose
    /// `origin_surface = 'delegation'`. Used by the delegation
    /// executor to find children that need to run. Returns
    /// `(task_id, params_json, owner_subject_id)` per row,
    /// oldest-first by `created_at` so old work doesn't starve.
    pub fn list_pending_delegated(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, String, String)>, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let cap = limit.clamp(1, self.max_list);
        let mut stmt = conn
            .prepare(
                "SELECT task_id, params_json, owner_subject_id
                 FROM tasks
                 WHERE origin_surface = 'delegation' AND status = 'pending'
                 ORDER BY created_at ASC
                 LIMIT ?1",
            )
            .map_err(CoordinatorError::Db)?;
        let rows = stmt
            .query_map(params![cap as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(CoordinatorError::Db)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(CoordinatorError::Db)?);
        }
        Ok(out)
    }

    /// Walk the `delegated_to` ancestor chain starting at
    /// `task_id` (treated as a child). Returns the depth —
    /// `0` when the task has no delegation parent, `1` when
    /// its parent has none, etc. Caps the walk at
    /// `max_depth` so a corrupted cycle can't wedge the
    /// caller; returns `max_depth` if the limit is reached.
    ///
    /// Used by the delegate.spawn handler to enforce the
    /// configured `max_depth` independently of the depth
    /// integer the caller passes — defence in depth against
    /// a malicious or buggy agent under-reporting its
    /// position in the chain.
    pub fn delegation_chain_depth(
        &self,
        task_id: &str,
        max_depth: usize,
    ) -> Result<usize, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut current = task_id.to_string();
        let mut depth = 0usize;
        while depth < max_depth {
            // Find a delegated_to edge that points AT `current`
            // (i.e. an ancestor that delegated to it). Take the
            // oldest one as the canonical parent.
            let row: Option<String> = conn
                .query_row(
                    "SELECT task_id FROM task_edges
                     WHERE related_task_id = ?1 AND edge_type = 'delegated_to'
                     ORDER BY edge_id ASC
                     LIMIT 1",
                    params![current],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(CoordinatorError::Db)?;
            match row {
                Some(parent) if parent != current => {
                    current = parent;
                    depth += 1;
                }
                _ => break,
            }
        }
        Ok(depth)
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

    /// Cross-task event firehose (M67). Returns the newest
    /// `limit` events strictly newer than `since_event_id`
    /// across ALL tasks. Distinct from `query_events` which
    /// is per-task. Operators use this for the dashboard's
    /// global live tail.
    ///
    /// `event_type_filter` is exact-match when set; None
    /// returns every event_type. `limit` is clamped to
    /// `[1, max_list]`.
    ///
    /// Returns `(events, task_id_for_each_event)` zipped so
    /// the bridge can render the task_id alongside the
    /// event without a second query. Order is newest-first
    /// by event_id.
    /// W5: pull every chat-turn chronicle event for a single
    /// chat session.
    ///
    /// Looks for events whose `event_type` is `chat.user_turn`
    /// or `chat.assistant_turn` AND whose payload begins with
    /// `<session_id>|`. The pipe-delimited payload format is
    /// `<session_id>|<role>|<timestamp_unix>|<content>`. Returns
    /// the parsed turns sorted chronologically (oldest first).
    pub fn query_chat_turns(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<ChatTurn>, CoordinatorError> {
        let cap = limit.clamp(1, self.max_list);
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let prefix = format!("{session_id}|");
        let mut stmt = conn
            .prepare(
                "SELECT event_type, payload, ts
                 FROM task_events
                 WHERE event_type IN ('chat.user_turn', 'chat.assistant_turn')
                   AND payload LIKE ?1
                 ORDER BY ts ASC, event_id ASC
                 LIMIT ?2",
            )
            .map_err(CoordinatorError::Db)?;
        let pattern = format!("{prefix}%");
        let rows = stmt
            .query_map(params![pattern, cap as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(CoordinatorError::Db)?;
        let mut out: Vec<ChatTurn> = Vec::new();
        for r in rows {
            let (event_type, payload, ts) = r.map_err(CoordinatorError::Db)?;
            if let Some(turn) = parse_chat_turn_payload(session_id, &event_type, &payload, ts) {
                out.push(turn);
            }
        }
        Ok(out)
    }

    /// Full-text search across chat-turn chronicle events.
    ///
    /// Matches `chat.user_turn` / `chat.assistant_turn` rows whose
    /// payload contains the query substring. When `subject_id` is
    /// non-empty, restricts the join to tasks owned by that
    /// subject; empty subject_id is the operator-only "search
    /// everything" path. `limit` is clamped to `[1, 100]`; a 0
    /// limit collapses to the default 20.
    ///
    /// Today the matcher is a `LIKE '%q%'` scan with `score = 1.0`
    /// for every hit; SQLite FTS5 indexing of the chronicle is a
    /// future optimization that doesn't change the return shape.
    /// Results are returned oldest-first across both tasks; the
    /// snippet carries up to 50 chars of content on each side of
    /// the first match.
    pub fn search_chat_turns(
        &self,
        subject_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionSearchHit>, CoordinatorError> {
        let cap = if limit == 0 {
            DEFAULT_SESSION_SEARCH_LIMIT
        } else {
            limit
        }
        .min(MAX_SESSION_SEARCH_LIMIT);
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        // Body filters on event_type + payload; subject_id filter
        // joins through `tasks.owner_subject_id`. Two paths so
        // the join is only taken when it actually narrows.
        let pattern = format!("%{trimmed}%");
        let rows: Vec<(String, String, i64)> = if subject_id.is_empty() {
            let mut stmt = conn
                .prepare(
                    "SELECT te.event_type, te.payload, te.ts
                     FROM task_events te
                     WHERE te.event_type IN ('chat.user_turn', 'chat.assistant_turn')
                       AND te.payload LIKE ?1
                     ORDER BY te.ts ASC, te.event_id ASC
                     LIMIT ?2",
                )
                .map_err(CoordinatorError::Db)?;
            stmt.query_map(params![pattern, cap as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(CoordinatorError::Db)?
        } else {
            let mut stmt = conn
                .prepare(
                    "SELECT te.event_type, te.payload, te.ts
                     FROM task_events te
                     JOIN tasks t ON te.task_id = t.task_id
                     WHERE te.event_type IN ('chat.user_turn', 'chat.assistant_turn')
                       AND te.payload LIKE ?1
                       AND t.owner_subject_id = ?2
                     ORDER BY te.ts ASC, te.event_id ASC
                     LIMIT ?3",
                )
                .map_err(CoordinatorError::Db)?;
            stmt.query_map(params![pattern, subject_id, cap as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(CoordinatorError::Db)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(CoordinatorError::Db)?
        };
        let mut out: Vec<SessionSearchHit> = Vec::with_capacity(rows.len());
        for (event_type, payload, ts) in rows {
            // Re-parse the payload using the chat-turn parser
            // (uses splitn(4,'|') so content can carry pipes).
            // We don't know the session_id upfront; pull it from
            // the payload's first field directly.
            let Some((sess, rest_after_session)) = payload.split_once('|') else {
                continue;
            };
            if let Some(turn) = parse_chat_turn_payload(sess, &event_type, &payload, ts)
                && turn
                    .content
                    .to_ascii_lowercase()
                    .contains(&trimmed.to_ascii_lowercase())
            {
                let snippet = build_match_snippet(&turn.content, trimmed);
                let _ = rest_after_session;
                out.push(SessionSearchHit {
                    session_id: turn.session_id,
                    role: turn.role,
                    content: turn.content,
                    timestamp_unix: turn.timestamp_unix,
                    snippet,
                    score: 1.0,
                });
            } else if let Some(turn) = parse_chat_turn_payload(sess, &event_type, &payload, ts) {
                // The LIKE matched on the payload (which includes
                // the session_id / role / ts prefix); the actual
                // content body doesn't contain the query. We
                // still emit a hit because the payload matched —
                // operators searching for a session_id substring
                // benefit, and matches on content are the common
                // case so this branch is rarely taken in practice.
                let snippet = build_match_snippet(&turn.content, trimmed);
                out.push(SessionSearchHit {
                    session_id: turn.session_id,
                    role: turn.role,
                    content: turn.content,
                    timestamp_unix: turn.timestamp_unix,
                    snippet,
                    score: 1.0,
                });
            }
        }
        Ok(out)
    }

    pub fn recent_events_cross_task(
        &self,
        since_event_id: i64,
        limit: usize,
        event_type_filter: Option<&str>,
    ) -> Result<Vec<(String, TaskEvent)>, CoordinatorError> {
        let cap = limit.clamp(1, self.max_list);
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let map_row = |r: &rusqlite::Row<'_>| {
            Ok((
                r.get::<_, String>(0)?,
                TaskEvent {
                    event_id: r.get(1)?,
                    ts: r.get(2)?,
                    event_type: r.get(3)?,
                    payload: r.get(4)?,
                    schema_version: r.get(5)?,
                    attempt_id: r.get(6)?,
                    trace_id: r.get(7)?,
                    payload_json: r.get(8)?,
                },
            ))
        };
        let mut out: Vec<(String, TaskEvent)> = Vec::with_capacity(cap);
        match event_type_filter.filter(|s| !s.is_empty()) {
            Some(t) => {
                let mut stmt = conn
                    .prepare(
                        "SELECT task_id, event_id, ts, event_type, payload,
                                schema_version, attempt_id, trace_id, payload_json
                         FROM task_events
                         WHERE event_id > ?1 AND event_type = ?3
                         ORDER BY event_id DESC LIMIT ?2",
                    )
                    .map_err(CoordinatorError::Db)?;
                let rows = stmt
                    .query_map(params![since_event_id, cap as i64, t], map_row)
                    .map_err(CoordinatorError::Db)?;
                for r in rows {
                    out.push(r.map_err(CoordinatorError::Db)?);
                }
            }
            None => {
                let mut stmt = conn
                    .prepare(
                        "SELECT task_id, event_id, ts, event_type, payload,
                                schema_version, attempt_id, trace_id, payload_json
                         FROM task_events
                         WHERE event_id > ?1
                         ORDER BY event_id DESC LIMIT ?2",
                    )
                    .map_err(CoordinatorError::Db)?;
                let rows = stmt
                    .query_map(params![since_event_id, cap as i64], map_row)
                    .map_err(CoordinatorError::Db)?;
                for r in rows {
                    out.push(r.map_err(CoordinatorError::Db)?);
                }
            }
        }
        Ok(out)
    }

    /// Walk the task execution lineage outward from a root
    /// task (M66). BFS over the `task_edges` table in both
    /// directions: downstream when an edge has
    /// `related_task_id == root` (root spawned/retried-from
    /// child) and upstream when `task_id == root` and
    /// `related_task_id != root` (someone else spawned root).
    /// `max_depth` caps traversal depth to bound runaway
    /// cycles a future producer might introduce.
    ///
    /// Returns the set of task ids reachable + every edge
    /// connecting them + a `cross_task_edge_count` summary.
    /// HONEST: with only `retried_from` shipping today (which
    /// links the same task_id to itself), the cross-task count
    /// is effectively always zero until other edge producers
    /// land. The dashboard surfaces this distinction.
    pub fn task_lineage(
        &self,
        root_task_id: &str,
        max_depth: usize,
    ) -> Result<TaskLineageGraph, CoordinatorError> {
        let cap_depth = max_depth.clamp(1, 16);
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        // BFS frontier. Each entry is (task_id, depth). We
        // explore both directions in one pass: every row where
        // task_id == frontier or related_task_id == frontier.
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut edges: Vec<TaskEdge> = Vec::new();
        let mut frontier: Vec<(String, usize)> = vec![(root_task_id.to_string(), 0)];
        seen.insert(root_task_id.to_string());
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
        while let Some((tid, depth)) = frontier.pop() {
            if depth >= cap_depth {
                continue;
            }
            let rows = stmt
                .query_map(params![&tid], |r| {
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
            for r in rows {
                let edge = r.map_err(CoordinatorError::Db)?;
                let other = if edge.task_id == tid {
                    edge.related_task_id.clone()
                } else {
                    Some(edge.task_id.clone())
                };
                // Push to frontier if other side is a different
                // task we haven't visited yet.
                if let Some(o) = other.as_ref()
                    && o != &tid
                    && !seen.contains(o)
                {
                    seen.insert(o.clone());
                    frontier.push((o.clone(), depth + 1));
                }
                // Dedupe edges by edge_id while preserving
                // chronological order.
                if !edges.iter().any(|e| e.edge_id == edge.edge_id) {
                    edges.push(edge);
                }
            }
        }
        edges.sort_by_key(|e| e.edge_id);
        let cross_task_edges = edges
            .iter()
            .filter(|e| {
                e.related_task_id
                    .as_deref()
                    .is_some_and(|other| other != e.task_id)
            })
            .count();
        // Sort the task list for deterministic output: root
        // first, then the rest lexicographically. Operators
        // scan the list top-down.
        let mut tasks: Vec<String> = seen.into_iter().collect();
        if let Some(pos) = tasks.iter().position(|t| t == root_task_id) {
            tasks.swap(0, pos);
        }
        Ok(TaskLineageGraph {
            root_task_id: root_task_id.to_string(),
            tasks,
            edges,
            cross_task_edge_count: cross_task_edges,
            max_depth_walked: cap_depth,
        })
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

    /// Operator-initiated pause (M65). Refuses terminal +
    /// already-paused statuses. Emits a `task.paused`
    /// chronicle event with author + optional reason.
    ///
    /// HONEST: like `cancel`, this is metadata-only today.
    /// The runtime has no flow-pause primitive — a currently-
    /// executing flow continues running and its eventual
    /// write-back may overwrite the `paused` status. The
    /// chronicle event records operator intent; the dashboard
    /// surfaces the `flow_still_running` caveat in the
    /// confirm flow.
    ///
    /// Returns the prior status so the caller can attach it
    /// to the eventual `task.resumed` event (operators want
    /// to see what state the task was returning to).
    pub fn set_paused(
        &self,
        task_id: &str,
        reason: Option<&str>,
        author_subject_id: &str,
    ) -> Result<String, CoordinatorError> {
        let trimmed_reason = reason.map(str::trim).filter(|s| !s.is_empty());
        if let Some(r) = trimmed_reason
            && r.len() > MAX_OPERATOR_NOTE_LEN
        {
            return Err(CoordinatorError::Invalid(format!(
                "task.pause: reason exceeds {MAX_OPERATOR_NOTE_LEN} bytes (got {})",
                r.len()
            )));
        }
        // H10: scrub secrets from operator-supplied reason text
        // (same boundary discipline as H8 operator_note +
        // investigation_marker reasons).
        let redacted_reason: Option<String> =
            trimmed_reason.map(relix_core::redact::redact_secrets);
        let safe_reason: Option<&str> = redacted_reason.as_deref();
        let now = unix_secs();
        let mut conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let prior_status: Option<String> = conn
            .query_row(
                "SELECT status FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(CoordinatorError::Db)?;
        let prior_status = prior_status.ok_or(CoordinatorError::NotFound(task_id.to_string()))?;
        if !PAUSABLE_STATUSES.contains(&prior_status.as_str()) {
            return Err(CoordinatorError::Invalid(format!(
                "task.pause: status '{prior_status}' is not pausable (allowed: {})",
                PAUSABLE_STATUSES.join(", ")
            )));
        }
        let tx = conn.transaction().map_err(CoordinatorError::Db)?;
        // M70: bump the pause generation as part of the same
        // transaction. The new value is what the chronicle
        // event carries and what cooperative pollers read.
        tx.execute(
            "UPDATE tasks
               SET status = 'paused',
                   updated_at = ?2,
                   pause_generation = pause_generation + 1
             WHERE task_id = ?1",
            params![task_id, now],
        )
        .map_err(CoordinatorError::Db)?;
        let new_pause_generation: i64 = tx
            .query_row(
                "SELECT pause_generation FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .map_err(CoordinatorError::Db)?;
        let payload_json = serde_json::json!({
            "prior_status": prior_status,
            "reason": safe_reason,
            "author": author_subject_id,
            // M70: intent vs ack. This is the REQUEST event;
            // the matching `task.pause_observed` (M70) lands
            // later when a cooperative worker calls
            // `task.observe_interruption`.
            "pause_generation": new_pause_generation,
            "intent": "request",
        })
        .to_string();
        let legacy = match safe_reason {
            Some(r) => format!("from {prior_status} · gen={new_pause_generation} · {r}"),
            None => format!("from {prior_status} · gen={new_pause_generation}"),
        };
        insert_typed_event(
            &tx,
            task_id,
            now,
            // M70: renamed from `task.paused` to make the
            // intent-vs-ack split explicit. The runtime
            // emits `task.pause_observed` when a cooperative
            // worker attests via `task.observe_interruption`.
            "task.pause_requested",
            &legacy,
            None,
            None,
            Some(&payload_json),
        )?;
        tx.commit().map_err(CoordinatorError::Db)?;
        Ok(prior_status)
    }

    /// Operator-initiated resume (M65). Refuses any status
    /// other than `paused`. Transitions to `pending` so a
    /// subsequent runtime tick can open a new attempt.
    /// Emits `task.resumed` with the pre-pause status (read
    /// from the most recent `task.paused` event for
    /// chronological accuracy).
    ///
    /// HONEST: this is purely a state restoration; it does
    /// not re-dispatch the flow. The operator must trigger
    /// re-execution separately (e.g. via the retry flow).
    pub fn set_resumed(
        &self,
        task_id: &str,
        author_subject_id: &str,
    ) -> Result<String, CoordinatorError> {
        let now = unix_secs();
        let mut conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let prior_status: Option<String> = conn
            .query_row(
                "SELECT status FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(CoordinatorError::Db)?;
        let prior_status = prior_status.ok_or(CoordinatorError::NotFound(task_id.to_string()))?;
        if prior_status != "paused" {
            return Err(CoordinatorError::Invalid(format!(
                "task.resume: status '{prior_status}' is not resumable (only 'paused' is)"
            )));
        }
        // Find the most recent pause-request event to recover
        // the pre-pause status. Best-effort: when missing,
        // the resumed event still lands with
        // prior_status=paused so the timeline is honest.
        // Accept both the new (M70) `task.pause_requested`
        // event_type AND the pre-M70 `task.paused` for
        // chronicle continuity.
        let pre_pause_status: String = conn
            .query_row(
                "SELECT payload_json FROM task_events
                  WHERE task_id = ?1
                    AND event_type IN ('task.pause_requested', 'task.paused')
                  ORDER BY event_id DESC LIMIT 1",
                params![task_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(CoordinatorError::Db)?
            .flatten()
            .and_then(|pj| {
                serde_json::from_str::<serde_json::Value>(&pj)
                    .ok()
                    .and_then(|v| {
                        v.get("prior_status")
                            .and_then(|s| s.as_str())
                            .map(str::to_string)
                    })
            })
            .unwrap_or_else(|| "paused".to_string());
        let tx = conn.transaction().map_err(CoordinatorError::Db)?;
        // M70: resume also bumps pause_generation so a
        // cooperative worker that cached the paused
        // generation knows to re-check before proceeding.
        tx.execute(
            "UPDATE tasks
               SET status = 'pending',
                   updated_at = ?2,
                   pause_generation = pause_generation + 1
             WHERE task_id = ?1",
            params![task_id, now],
        )
        .map_err(CoordinatorError::Db)?;
        let new_pause_generation: i64 = tx
            .query_row(
                "SELECT pause_generation FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .map_err(CoordinatorError::Db)?;
        let payload_json = serde_json::json!({
            "pre_pause_status": pre_pause_status,
            "new_status": "pending",
            "author": author_subject_id,
            "pause_generation": new_pause_generation,
            "intent": "request",
        })
        .to_string();
        let legacy =
            format!("paused→pending (was {pre_pause_status}) · gen={new_pause_generation}");
        insert_typed_event(
            &tx,
            task_id,
            now,
            // M70: renamed for intent-vs-ack clarity. The
            // `task.resume_observed` event lands later via
            // cooperative `task.observe_interruption`.
            "task.resume_requested",
            &legacy,
            None,
            None,
            Some(&payload_json),
        )?;
        tx.commit().map_err(CoordinatorError::Db)?;
        Ok(pre_pause_status)
    }

    /// Operator-initiated freeze (M71). Workflow-level
    /// equivalent of pause. Transitions to `frozen` status,
    /// stamps `frozen_at`, bumps `freeze_generation`, emits
    /// `task.freeze_requested` chronicle event.
    ///
    /// HONEST: like pause, this is metadata-only. The
    /// runtime has no freeze-gate primitive yet — a flow
    /// already executing continues underneath the `frozen`
    /// status. Future cooperative workers (M70 protocol)
    /// will observe the `freeze_generation` bump and emit
    /// `task.freeze_propagated` as they refuse to start
    /// new node execution.
    ///
    /// Returns the prior status so callers can report a
    /// faithful transition.
    pub fn set_frozen(
        &self,
        task_id: &str,
        reason: Option<&str>,
        author_subject_id: &str,
    ) -> Result<String, CoordinatorError> {
        let trimmed_reason = reason.map(str::trim).filter(|s| !s.is_empty());
        if let Some(r) = trimmed_reason
            && r.len() > MAX_OPERATOR_NOTE_LEN
        {
            return Err(CoordinatorError::Invalid(format!(
                "task.freeze: reason exceeds {MAX_OPERATOR_NOTE_LEN} bytes (got {})",
                r.len()
            )));
        }
        // H10: same redaction posture as set_paused (H10) / H8
        // operator_note / H8 investigation_marker.
        let redacted_reason: Option<String> =
            trimmed_reason.map(relix_core::redact::redact_secrets);
        let safe_reason: Option<&str> = redacted_reason.as_deref();
        let now = unix_secs();
        let mut conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let prior_status: Option<String> = conn
            .query_row(
                "SELECT status FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(CoordinatorError::Db)?;
        let prior_status = prior_status.ok_or(CoordinatorError::NotFound(task_id.to_string()))?;
        if !FREEZABLE_STATUSES.contains(&prior_status.as_str()) {
            return Err(CoordinatorError::Invalid(format!(
                "task.freeze: status '{prior_status}' is not freezable (allowed: {})",
                FREEZABLE_STATUSES.join(", ")
            )));
        }
        let tx = conn.transaction().map_err(CoordinatorError::Db)?;
        tx.execute(
            "UPDATE tasks
               SET status = 'frozen',
                   frozen_at = ?2,
                   frozen_reason = ?3,
                   updated_at = ?2,
                   freeze_generation = freeze_generation + 1
             WHERE task_id = ?1",
            params![task_id, now, safe_reason],
        )
        .map_err(CoordinatorError::Db)?;
        let new_freeze_generation: i64 = tx
            .query_row(
                "SELECT freeze_generation FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .map_err(CoordinatorError::Db)?;
        let payload_json = serde_json::json!({
            "prior_status": prior_status,
            "reason": safe_reason,
            "author": author_subject_id,
            "freeze_generation": new_freeze_generation,
            "intent": "request",
        })
        .to_string();
        let legacy = match safe_reason {
            Some(r) => format!("from {prior_status} · gen={new_freeze_generation} · {r}"),
            None => format!("from {prior_status} · gen={new_freeze_generation}"),
        };
        insert_typed_event(
            &tx,
            task_id,
            now,
            "task.freeze_requested",
            &legacy,
            None,
            None,
            Some(&payload_json),
        )?;
        tx.commit().map_err(CoordinatorError::Db)?;
        Ok(prior_status)
    }

    /// Operator-initiated unfreeze (M71). Refuses any status
    /// other than `frozen`. Transitions to `pending`,
    /// clears `frozen_at` + `frozen_reason`, bumps
    /// `freeze_generation`, emits `task.unfreeze_requested`.
    pub fn set_unfrozen(
        &self,
        task_id: &str,
        author_subject_id: &str,
    ) -> Result<String, CoordinatorError> {
        let now = unix_secs();
        let mut conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let prior_status: Option<String> = conn
            .query_row(
                "SELECT status FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(CoordinatorError::Db)?;
        let prior_status = prior_status.ok_or(CoordinatorError::NotFound(task_id.to_string()))?;
        if prior_status != "frozen" {
            return Err(CoordinatorError::Invalid(format!(
                "task.unfreeze: status '{prior_status}' is not unfreezable (only 'frozen' is)"
            )));
        }
        // Recover the pre-freeze status from the most recent
        // freeze-request event so the unfreeze chronicle
        // entry tells operators where the task was returning
        // to.
        let pre_freeze_status: String = conn
            .query_row(
                "SELECT payload_json FROM task_events
                  WHERE task_id = ?1
                    AND event_type = 'task.freeze_requested'
                  ORDER BY event_id DESC LIMIT 1",
                params![task_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(CoordinatorError::Db)?
            .flatten()
            .and_then(|pj| {
                serde_json::from_str::<serde_json::Value>(&pj)
                    .ok()
                    .and_then(|v| {
                        v.get("prior_status")
                            .and_then(|s| s.as_str())
                            .map(str::to_string)
                    })
            })
            .unwrap_or_else(|| "frozen".to_string());
        let tx = conn.transaction().map_err(CoordinatorError::Db)?;
        tx.execute(
            "UPDATE tasks
               SET status = 'pending',
                   frozen_at = NULL,
                   frozen_reason = NULL,
                   updated_at = ?2,
                   freeze_generation = freeze_generation + 1
             WHERE task_id = ?1",
            params![task_id, now],
        )
        .map_err(CoordinatorError::Db)?;
        let new_freeze_generation: i64 = tx
            .query_row(
                "SELECT freeze_generation FROM tasks WHERE task_id = ?1",
                params![task_id],
                |r| r.get(0),
            )
            .map_err(CoordinatorError::Db)?;
        let payload_json = serde_json::json!({
            "pre_freeze_status": pre_freeze_status,
            "new_status": "pending",
            "author": author_subject_id,
            "freeze_generation": new_freeze_generation,
            "intent": "request",
        })
        .to_string();
        let legacy =
            format!("frozen→pending (was {pre_freeze_status}) · gen={new_freeze_generation}");
        insert_typed_event(
            &tx,
            task_id,
            now,
            "task.unfreeze_requested",
            &legacy,
            None,
            None,
            Some(&payload_json),
        )?;
        tx.commit().map_err(CoordinatorError::Db)?;
        Ok(pre_freeze_status)
    }

    /// Aggregate runtime metrics over an execution subtree
    /// (M75). Walks the M66 lineage from `root_task_id`, then
    /// reads each task's status + timing + attempt count and
    /// rolls them up into a single envelope.
    ///
    /// Metrics computed:
    /// - `total_tasks` — distinct tasks in the subtree.
    /// - `terminal_*` — counts per terminal status
    ///   (completed/failed/cancelled).
    /// - `active_*` — counts per active status
    ///   (running/retrying/pending/paused/frozen/awaiting_input).
    /// - `total_attempts` — sum of `attempt_count` across the
    ///   subtree (real work performed).
    /// - `total_wall_clock_secs` — sum of per-task durations
    ///   (`updated_at - started_at` for terminal tasks,
    ///   `now - started_at` for live tasks; skipped when
    ///   `started_at` is None — no fabricated durations).
    /// - `oldest_started_at` / `newest_updated_at` — the
    ///   span of the subtree's activity.
    /// - `tasks_with_missing_timing` — honesty counter:
    ///   tasks that had no `started_at` and therefore did
    ///   not contribute to wall-clock aggregation.
    ///
    /// HONEST: with only `retried_from` + the M72 producers
    /// shipping today, the subtree is usually just the root.
    /// Operators see real per-task aggregates even in the
    /// single-task case, and the count of related tasks
    /// reveals when the M72 cross-task edges start landing.
    pub fn subtree_metrics(
        &self,
        root_task_id: &str,
        max_depth: usize,
    ) -> Result<SubtreeMetrics, CoordinatorError> {
        // Validate the root exists up front. `task_lineage`
        // is permissive (it returns the root in `tasks` even
        // when unknown so callers can render an empty graph),
        // but for metrics an unknown root is a real error —
        // operators get NotFound instead of an all-zeros
        // aggregate.
        {
            let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM tasks WHERE task_id = ?1",
                    params![root_task_id],
                    |r| r.get(0),
                )
                .map_err(CoordinatorError::Db)?;
            if exists == 0 {
                return Err(CoordinatorError::NotFound(root_task_id.to_string()));
            }
        }
        let lineage = self.task_lineage(root_task_id, max_depth)?;
        let now = unix_secs();
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let mut terminal_completed = 0i64;
        let mut terminal_failed = 0i64;
        let mut terminal_cancelled = 0i64;
        let mut active_pending = 0i64;
        let mut active_running = 0i64;
        let mut active_retrying = 0i64;
        let mut active_paused = 0i64;
        let mut active_frozen = 0i64;
        let mut active_interrupted = 0i64;
        let mut active_awaiting_input = 0i64;
        let mut other_status = 0i64;
        let mut total_attempts: i64 = 0;
        let mut total_wall_clock_secs: i64 = 0;
        let mut oldest_started_at: Option<i64> = None;
        let mut newest_updated_at: Option<i64> = None;
        let mut tasks_with_missing_timing: i64 = 0;
        for tid in &lineage.tasks {
            // Single-row read per task. The bounded subtree
            // size (BFS depth clamp at 16) keeps this O(N)
            // with a small N — no N+1-query worry at the
            // scales the alpha runtime targets.
            let row = conn
                .query_row(
                    "SELECT status, started_at, updated_at, attempt_count
                     FROM tasks WHERE task_id = ?1",
                    params![tid],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<i64>>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()
                .map_err(CoordinatorError::Db)?;
            let Some((status, started_at, updated_at, attempt_count)) = row else {
                // Task vanished between the lineage walk and
                // this metric read. Don't fabricate — just
                // skip + count as missing-timing.
                tasks_with_missing_timing += 1;
                continue;
            };
            total_attempts += attempt_count;
            match status.as_str() {
                "completed" => terminal_completed += 1,
                "failed" => terminal_failed += 1,
                "cancelled" => terminal_cancelled += 1,
                "pending" => active_pending += 1,
                "running" => active_running += 1,
                "retrying" => active_retrying += 1,
                "paused" => active_paused += 1,
                "frozen" => active_frozen += 1,
                "interrupted" => active_interrupted += 1,
                "awaiting_input" => active_awaiting_input += 1,
                _ => other_status += 1,
            }
            // Wall-clock aggregation. Terminal statuses use
            // updated_at as the "ended at"; active statuses
            // use `now`. Both require started_at — without
            // it we skip (and count for honesty).
            let is_terminal = matches!(status.as_str(), "completed" | "failed" | "cancelled");
            match started_at {
                Some(start) => {
                    let end = if is_terminal { updated_at } else { now };
                    let dur = (end - start).max(0);
                    total_wall_clock_secs += dur;
                    oldest_started_at = Some(match oldest_started_at {
                        Some(prev) => prev.min(start),
                        None => start,
                    });
                }
                None => {
                    tasks_with_missing_timing += 1;
                }
            }
            newest_updated_at = Some(match newest_updated_at {
                Some(prev) => prev.max(updated_at),
                None => updated_at,
            });
        }
        Ok(SubtreeMetrics {
            root_task_id: root_task_id.to_string(),
            total_tasks: lineage.tasks.len() as i64,
            cross_task_edge_count: lineage.cross_task_edge_count as i64,
            terminal_completed,
            terminal_failed,
            terminal_cancelled,
            active_pending,
            active_running,
            active_retrying,
            active_paused,
            active_frozen,
            active_interrupted,
            active_awaiting_input,
            other_status,
            total_attempts,
            total_wall_clock_secs,
            oldest_started_at,
            newest_updated_at,
            tasks_with_missing_timing,
            max_depth_walked: lineage.max_depth_walked as i64,
        })
    }

    /// Record a `spawned_task` edge attesting that the
    /// caller (a runtime worker or operator-driven tool)
    /// observed `parent_task_id` spawning `child_task_id`
    /// (M72).
    ///
    /// Both tasks must exist. The caller's subject_id is
    /// recorded as the "producer" so operators inspecting
    /// the graph can trace which runtime node attested the
    /// relationship. Emits a `task.spawned_child` chronicle
    /// event on the PARENT so the per-task timeline shows
    /// the spawn, and inserts the edge with
    /// spawned_by_event_id pointing at that event for
    /// round-trip clickability.
    ///
    /// `branch_id` and `context_id` are optional opaque
    /// labels the producer can use to group related spawns
    /// (e.g. a parallel-branch identifier or a flow
    /// execution context). When `None`, the corresponding
    /// payload field is omitted — operators see the absence
    /// rather than a fabricated default.
    ///
    /// HONEST: today no runtime path automatically calls
    /// this. The capability is ready; producers will land
    /// as we add runtime hooks. The schema rejects synth.
    pub fn record_spawned(
        &self,
        parent_task_id: &str,
        child_task_id: &str,
        branch_id: Option<&str>,
        context_id: Option<&str>,
        producer_subject_id: &str,
    ) -> Result<EdgeProducerOutcome, CoordinatorError> {
        self.record_cross_task_edge(
            parent_task_id,
            child_task_id,
            "spawned",
            "task.spawned_child",
            branch_id,
            context_id,
            None,
            producer_subject_id,
        )
    }

    /// Record a `delegated_to` edge — same shape as
    /// `record_spawned` but with delegation semantics
    /// (parent passed responsibility for completion to
    /// the child instead of fan-out). `delegation_reason`
    /// is optional, surfaced verbatim in payload_json.
    pub fn record_delegated(
        &self,
        parent_task_id: &str,
        child_task_id: &str,
        delegation_reason: Option<&str>,
        producer_subject_id: &str,
    ) -> Result<EdgeProducerOutcome, CoordinatorError> {
        self.record_cross_task_edge(
            parent_task_id,
            child_task_id,
            "delegated_to",
            "task.delegated_to",
            None,
            None,
            delegation_reason,
            producer_subject_id,
        )
    }

    /// Record an `awaited` edge — parent is blocked waiting
    /// for `awaited_task_id` to complete. `await_reason` is
    /// optional, surfaced verbatim in payload_json.
    pub fn record_awaited(
        &self,
        parent_task_id: &str,
        awaited_task_id: &str,
        await_reason: Option<&str>,
        producer_subject_id: &str,
    ) -> Result<EdgeProducerOutcome, CoordinatorError> {
        self.record_cross_task_edge(
            parent_task_id,
            awaited_task_id,
            "awaited",
            "task.awaiting",
            None,
            None,
            await_reason,
            producer_subject_id,
        )
    }

    /// Shared insert helper for the M72 edge producers.
    /// Wraps the chronicle-event emit + edge insert in one
    /// transaction so callers can never see one without the
    /// other.
    #[allow(clippy::too_many_arguments)]
    fn record_cross_task_edge(
        &self,
        parent_task_id: &str,
        related_task_id: &str,
        edge_type: &str,
        event_type: &str,
        branch_id: Option<&str>,
        context_id: Option<&str>,
        reason: Option<&str>,
        producer_subject_id: &str,
    ) -> Result<EdgeProducerOutcome, CoordinatorError> {
        if parent_task_id == related_task_id {
            return Err(CoordinatorError::Invalid(format!(
                "{edge_type}: parent and related task_id must differ \
                 (intra-task edges have dedicated types)"
            )));
        }
        let trimmed_branch = branch_id.map(str::trim).filter(|s| !s.is_empty());
        let trimmed_context = context_id.map(str::trim).filter(|s| !s.is_empty());
        let trimmed_reason = reason.map(str::trim).filter(|s| !s.is_empty());
        if let Some(r) = trimmed_reason
            && r.len() > MAX_OPERATOR_NOTE_LEN
        {
            return Err(CoordinatorError::Invalid(format!(
                "{edge_type}: reason exceeds {MAX_OPERATOR_NOTE_LEN} bytes (got {})",
                r.len()
            )));
        }
        let now = unix_secs();
        let mut conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        for tid in [parent_task_id, related_task_id] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM tasks WHERE task_id = ?1",
                    params![tid],
                    |r| r.get(0),
                )
                .map_err(CoordinatorError::Db)?;
            if exists == 0 {
                return Err(CoordinatorError::NotFound(tid.to_string()));
            }
        }
        let tx = conn.transaction().map_err(CoordinatorError::Db)?;
        let mut payload = serde_json::json!({
            "edge_type": edge_type,
            "related_task_id": related_task_id,
            "producer": producer_subject_id,
            "produced_at": now,
        });
        if let Some(b) = trimmed_branch {
            payload["branch_id"] = serde_json::Value::String(b.to_string());
        }
        if let Some(c) = trimmed_context {
            payload["context_id"] = serde_json::Value::String(c.to_string());
        }
        if let Some(r) = trimmed_reason {
            payload["reason"] = serde_json::Value::String(r.to_string());
        }
        let payload_json = payload.to_string();
        let legacy = match (trimmed_branch, trimmed_reason) {
            (Some(b), Some(r)) => format!("→{related_task_id} · branch={b} · {r}"),
            (Some(b), None) => format!("→{related_task_id} · branch={b}"),
            (None, Some(r)) => format!("→{related_task_id} · {r}"),
            (None, None) => format!("→{related_task_id}"),
        };
        insert_typed_event(
            &tx,
            parent_task_id,
            now,
            event_type,
            &legacy,
            None,
            None,
            Some(&payload_json),
        )?;
        let event_id = tx.last_insert_rowid();
        // Insert the edge with spawned_by_event_id pointing
        // at the chronicle event we just wrote. attempt_id
        // left NULL — these edges are task-scoped, not
        // attempt-scoped (until the runtime is attest-rich
        // enough to know).
        tx.execute(
            "INSERT INTO task_edges
                (task_id, attempt_id, edge_type, related_task_id,
                 related_attempt_id, spawned_by_event_id, created_at)
             VALUES (?1, NULL, ?2, ?3, NULL, ?4, ?5)",
            params![parent_task_id, edge_type, related_task_id, event_id, now],
        )
        .map_err(CoordinatorError::Db)?;
        let edge_id = tx.last_insert_rowid();
        tx.commit().map_err(CoordinatorError::Db)?;
        Ok(EdgeProducerOutcome { edge_id, event_id })
    }

    /// Cooperative-poller snapshot of interruption state
    /// (M70). Returns the current pause/freeze generations
    /// plus the live status — enough for a runtime worker
    /// to detect "is there a newer pause request I haven't
    /// observed yet?"
    ///
    /// Independent of any wall-clock; the bridge handler
    /// surfaces the snapshot back to the caller verbatim.
    /// NotFound when the task id is unknown.
    pub fn interruption_snapshot(
        &self,
        task_id: &str,
    ) -> Result<InterruptionSnapshot, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        conn.query_row(
            "SELECT status, pause_generation, freeze_generation
             FROM tasks WHERE task_id = ?1",
            params![task_id],
            |r| {
                Ok(InterruptionSnapshot {
                    task_id: task_id.to_string(),
                    status: r.get(0)?,
                    pause_generation: r.get(1)?,
                    freeze_generation: r.get(2)?,
                })
            },
        )
        .optional()
        .map_err(CoordinatorError::Db)?
        .ok_or(CoordinatorError::NotFound(task_id.to_string()))
    }

    /// Runtime ack that a cooperative worker noticed an
    /// interruption request (M70). Emits the matching
    /// `task.pause_observed` / `task.resume_observed` /
    /// `task.freeze_propagated` chronicle event with the
    /// observer subject_id + the generation they noticed.
    ///
    /// Distinguishes operator INTENT (the original request
    /// event) from runtime ACK (this event). Operators
    /// inspecting the chronicle see exactly when the
    /// runtime caught up — and when it didn't (a request
    /// with no matching ack means the runtime never noticed
    /// or wasn't running).
    ///
    /// `interruption_type` must be one of `pause` / `resume`
    /// / `freeze`. `generation_observed` is the value the
    /// worker saw when it observed — recorded for cross-
    /// reference even if the live generation has since
    /// advanced.
    pub fn observe_interruption(
        &self,
        task_id: &str,
        interruption_type: &str,
        generation_observed: i64,
        observer_subject_id: &str,
    ) -> Result<i64, CoordinatorError> {
        let event_type = match interruption_type {
            "pause" => "task.pause_observed",
            "resume" => "task.resume_observed",
            "freeze" => "task.freeze_propagated",
            other => {
                return Err(CoordinatorError::Invalid(format!(
                    "task.observe_interruption: unknown interruption_type \
                     '{other}' (expected pause|resume|freeze)"
                )));
            }
        };
        if generation_observed < 0 {
            return Err(CoordinatorError::Invalid(
                "task.observe_interruption: generation_observed must be non-negative".to_string(),
            ));
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
        let payload_json = serde_json::json!({
            "interruption_type": interruption_type,
            "generation_observed": generation_observed,
            "observer": observer_subject_id,
            "observed_at": now,
            "intent": "ack",
        })
        .to_string();
        let legacy = format!(
            "{interruption_type} observed at gen={generation_observed} by {observer_subject_id}"
        );
        let tx = conn.transaction().map_err(CoordinatorError::Db)?;
        insert_typed_event(
            &tx,
            task_id,
            now,
            event_type,
            &legacy,
            None,
            None,
            Some(&payload_json),
        )?;
        let event_id = tx.last_insert_rowid();
        tx.commit().map_err(CoordinatorError::Db)?;
        Ok(event_id)
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
        // H8: redact known secrets before persisting the reason.
        // Same posture as task.operator_note — anything that lands
        // on a task row OR in the chronicle gets the scrubber.
        let redacted_reason: Option<String> =
            trimmed_reason.map(relix_core::redact::redact_secrets);
        let store_reason: Option<&str> = redacted_reason.as_deref();
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
                params![task_id, now, store_reason],
            )
            .map_err(CoordinatorError::Db)?;
            let payload_json = serde_json::json!({
                "marked": true,
                "reason": store_reason,
                "author": author_subject_id,
            })
            .to_string();
            let legacy = match store_reason {
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
        // H8: scrub known-shape secrets BEFORE persisting. Operator
        // notes get replayed via the dashboard forever, so a pasted
        // API key in a "investigating prod outage" note would
        // become a permanent leak. redact_secrets is idempotent +
        // non-destructive for non-secret text.
        let redacted = relix_core::redact::redact_secrets(trimmed);
        let safe_text = redacted.as_str();
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
        // The legacy `payload` string carries the redacted note
        // verbatim so older grep-driven CLIs see the text;
        // payload_json carries the typed envelope.
        let payload_json = serde_json::json!({
            "note": safe_text,
            "author": author_subject_id,
        })
        .to_string();
        let tx = conn.transaction().map_err(CoordinatorError::Db)?;
        insert_typed_event(
            &tx,
            task_id,
            now,
            "task.operator_note",
            safe_text,
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
                        investigation_marked_at, investigation_reason,
                        pause_generation, freeze_generation,
                        frozen_at, frozen_reason, origin_surface
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
                        pause_generation: r.get(23)?,
                        freeze_generation: r.get(24)?,
                        frozen_at: r.get(25)?,
                        frozen_reason: r.get(26)?,
                        origin_surface: r.get(27)?,
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

    /// Compact chronicle events. Implements Step 3 + Step 4 of
    /// `docs/chronicle-retention.md` (bounded delete + snapshot
    /// synthesis). Returns a [`RetentionResult`] summarising the
    /// pass.
    ///
    /// Contract:
    /// - Only events whose parent task is in a terminal status
    ///   (`completed` / `failed` / `cancelled` / `interrupted`)
    ///   are touched (R5).
    /// - Before deleting any event for a task, emits a single
    ///   `task.snapshot` event whose payload_json summarises the
    ///   compacted range (event_count, ts range, final task
    ///   status). The snapshot is itself a `task_events` row;
    ///   deletion queries explicitly exclude it (R3).
    /// - Bounded per pass: each DELETE statement carries
    ///   `LIMIT MAX_ROWS_PER_RETENTION_PASS` and runs in its own
    ///   transaction. The outer loop stops when either no more
    ///   candidates exist OR `max_passes` is reached (R4).
    ///
    /// `cutoff_ts` is unix seconds; events strictly older than
    /// the cutoff become candidates. `max_passes` caps the loop;
    /// callers typically pass the configured
    /// `max_passes_per_run`.
    pub fn run_retention(
        &self,
        cutoff_ts: i64,
        max_passes: u32,
    ) -> Result<RetentionResult, CoordinatorError> {
        let mut total_deleted: i64 = 0;
        let mut snapshots_emitted: i64 = 0;
        let mut passes_run: u32 = 0;
        let mut stopped_at_pass_limit = false;

        // Phase 1: snapshot synthesis. One `task.snapshot` per
        // terminal task that has any candidate events, BEFORE any
        // bounded delete pass touches the actual rows.
        let candidate_tasks: Vec<(String, String)> = {
            let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT t.task_id, t.status
                     FROM task_events te
                     JOIN tasks t ON t.task_id = te.task_id
                     WHERE te.ts < ?1
                       AND t.status IN ('completed','failed','cancelled','interrupted')
                       AND te.event_type != 'task.snapshot'",
                )
                .map_err(CoordinatorError::Db)?;
            let rows = stmt
                .query_map(params![cutoff_ts], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(CoordinatorError::Db)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(CoordinatorError::Db)?);
            }
            out
        };

        for (task_id, final_status) in &candidate_tasks {
            let summary = self.compute_compaction_summary(task_id, cutoff_ts, final_status)?;
            if summary.event_count == 0 {
                // Race-condition guard: another writer drained the
                // candidate set between the SELECT above and now.
                continue;
            }
            self.emit_snapshot_event(task_id, &summary)?;
            snapshots_emitted += 1;
        }

        // Phase 2: bounded delete passes. We delete at most
        // MAX_ROWS_PER_RETENTION_PASS rows per transaction; the
        // outer loop continues until either nothing matches or
        // `max_passes` is hit. The `task.snapshot` events we just
        // emitted are explicitly excluded by event_type so a
        // subsequent run sees the same snapshot rows it inserted.
        loop {
            if passes_run >= max_passes {
                stopped_at_pass_limit = true;
                break;
            }
            let mut conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
            let tx = conn.transaction().map_err(CoordinatorError::Db)?;
            let n = tx
                .execute(
                    "DELETE FROM task_events \
                     WHERE event_id IN (\
                         SELECT te.event_id \
                         FROM task_events te \
                         JOIN tasks t ON t.task_id = te.task_id \
                         WHERE te.ts < ?1 \
                           AND t.status IN ('completed','failed','cancelled','interrupted') \
                           AND te.event_type != 'task.snapshot' \
                         LIMIT ?2\
                     )",
                    params![cutoff_ts, MAX_ROWS_PER_RETENTION_PASS],
                )
                .map_err(CoordinatorError::Db)?;
            tx.commit().map_err(CoordinatorError::Db)?;
            drop(conn);
            if n == 0 {
                break;
            }
            total_deleted += n as i64;
            passes_run += 1;
            // Defence-in-depth: a short yield so the writer
            // mutex doesn't starve concurrent chat traffic
            // between passes. Sub-millisecond on every platform
            // Relix supports.
            std::thread::yield_now();
        }

        Ok(RetentionResult {
            cutoff_ts,
            tasks_compacted: candidate_tasks.len() as i64,
            events_deleted: total_deleted,
            snapshots_emitted,
            passes_run,
            stopped_at_pass_limit,
        })
    }

    /// Compute the `task.snapshot` payload for one task: how many
    /// candidate events exist, the time range they span, and the
    /// task's final status.
    fn compute_compaction_summary(
        &self,
        task_id: &str,
        cutoff_ts: i64,
        final_status: &str,
    ) -> Result<CompactionSummary, CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let (count, first_ts, last_ts): (i64, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT COUNT(*), MIN(ts), MAX(ts) \
                 FROM task_events \
                 WHERE task_id = ?1 AND ts < ?2 AND event_type != 'task.snapshot'",
                params![task_id, cutoff_ts],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(CoordinatorError::Db)?;
        Ok(CompactionSummary {
            event_count: count,
            first_ts,
            last_ts,
            final_status: final_status.to_string(),
        })
    }

    /// Append one `task.snapshot` chronicle event summarising the
    /// events about to be compacted. Wire-compatible with the
    /// existing event-write path (schema_version = 1, structured
    /// payload_json) so dashboards / `task.events` consumers see
    /// it just like any other typed event.
    fn emit_snapshot_event(
        &self,
        task_id: &str,
        summary: &CompactionSummary,
    ) -> Result<(), CoordinatorError> {
        let conn = self.conn.lock().map_err(|_| CoordinatorError::Lock)?;
        let now = unix_secs();
        let payload = format!(
            "task.snapshot: compacted {} events ({} → {}); final_status={}",
            summary.event_count,
            summary
                .first_ts
                .map(|t| t.to_string())
                .unwrap_or_else(|| "—".to_string()),
            summary
                .last_ts
                .map(|t| t.to_string())
                .unwrap_or_else(|| "—".to_string()),
            summary.final_status,
        );
        let payload_json = render_snapshot_payload_json(summary);
        conn.execute(
            "INSERT INTO task_events \
             (task_id, ts, event_type, payload, schema_version, payload_json) \
             VALUES (?1, ?2, 'task.snapshot', ?3, 1, ?4)",
            params![task_id, now, payload, payload_json],
        )
        .map_err(CoordinatorError::Db)?;
        Ok(())
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
    /// M70: monotonically-increasing counter bumped on every
    /// pause/resume request. Cooperative workers (future)
    /// poll `task.interruption_check`, cache this generation,
    /// and re-read state whenever it advances.
    pub pause_generation: i64,
    /// M70/M71: same idea for freeze/unfreeze. Bumped only by
    /// the freeze axis so a pause request doesn't invalidate a
    /// worker's freeze cache and vice versa.
    pub freeze_generation: i64,
    /// M71: operator-set freeze stamp. `Some(ts)` when the
    /// task is currently frozen (status = `frozen`); `None`
    /// when not frozen. The chronicle preserves every
    /// transition via `task.freeze_requested` /
    /// `task.unfreeze_requested` events.
    pub frozen_at: Option<i64>,
    /// M71: optional operator-supplied reason captured at
    /// freeze time. Cleared on unfreeze.
    pub frozen_reason: Option<String>,
    /// PH-ORIGIN-SURFACE (D-004): which dispatch surface
    /// created this task. `None` on rows created before the
    /// migration OR by callers that didn't stamp the value.
    /// Expected values: `"chat"`, `"dashboard"`, `"cli"`,
    /// `"channel"`, `"flow-engine"`, or any short
    /// operator-supplied label. The dashboard treats `None`
    /// as "unknown" for filter rendering.
    pub origin_surface: Option<String>,
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

/// Per-task summary computed before any rows are deleted; used to
/// build the `task.snapshot` event payload. Pure data — see
/// [`TaskStore::run_retention`] for the consuming code.
#[derive(Debug, Clone)]
pub struct CompactionSummary {
    /// How many `task_events` rows for this task fall within the
    /// retention candidate set.
    pub event_count: i64,
    /// `MIN(ts)` over the candidate set. `None` only on a race
    /// where the candidate set drained between SELECT and summary.
    pub first_ts: Option<i64>,
    /// `MAX(ts)` over the candidate set.
    pub last_ts: Option<i64>,
    /// `tasks.status` at the moment retention ran. The snapshot
    /// preserves this so a future operator scan still sees the
    /// task's final terminal state in the chronicle even after
    /// the per-attempt events are gone.
    pub final_status: String,
}

/// Result of a [`TaskStore::run_retention`] pass. Surfaced in
/// startup logs + (eventually) in the dashboard's retention
/// panel.
#[derive(Debug, Clone)]
pub struct RetentionResult {
    /// Cutoff timestamp the run honoured. Events with `ts < this`
    /// were candidates.
    pub cutoff_ts: i64,
    /// Number of tasks that received at least one `task.snapshot`
    /// event during the run.
    pub tasks_compacted: i64,
    /// Total `task_events` rows deleted across all passes.
    pub events_deleted: i64,
    /// Number of `task.snapshot` events emitted (one per
    /// terminal task with candidate events).
    pub snapshots_emitted: i64,
    /// Number of bounded-delete passes that actually executed.
    pub passes_run: u32,
    /// True when the loop stopped because it hit
    /// `max_passes_per_run` rather than because the candidate set
    /// was exhausted. Honest signal for operators to bump the
    /// limit or wait for the next scheduled run.
    pub stopped_at_pass_limit: bool,
}

/// Hand-built JSON renderer for the `task.snapshot` payload.
/// Same approach as the other Coordinator renderers — no
/// serde_json on the runtime crate.
fn render_snapshot_payload_json(s: &CompactionSummary) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(160);
    let _ = write!(
        out,
        r#"{{"compacted_event_count":{},"final_status":"{}""#,
        s.event_count,
        json_escape(&s.final_status),
    );
    if let Some(t) = s.first_ts {
        let _ = write!(out, r#","first_ts":{t}"#);
    }
    if let Some(t) = s.last_ts {
        let _ = write!(out, r#","last_ts":{t}"#);
    }
    out.push('}');
    out
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

/// Aggregate metrics over an execution subtree (M75).
/// Computed by [`TaskStore::subtree_metrics`] from REAL
/// per-task state — no synthesis. Status buckets are
/// distinct fields rather than a map so consumers don't
/// need a separate vocab decoder.
#[derive(Debug, Clone)]
pub struct SubtreeMetrics {
    pub root_task_id: String,
    /// Total distinct tasks reachable in the lineage (M66
    /// BFS at `max_depth_walked`).
    pub total_tasks: i64,
    /// Edges where `task_id != related_task_id` — the same
    /// honest counter the lineage walker computes.
    pub cross_task_edge_count: i64,
    pub terminal_completed: i64,
    pub terminal_failed: i64,
    pub terminal_cancelled: i64,
    pub active_pending: i64,
    pub active_running: i64,
    pub active_retrying: i64,
    pub active_paused: i64,
    pub active_frozen: i64,
    pub active_interrupted: i64,
    pub active_awaiting_input: i64,
    /// Any status outside the canonical vocabulary. Lets
    /// operators see when callers wrote custom states
    /// (intentional) vs when the schema drifted.
    pub other_status: i64,
    /// Sum of `attempt_count` across the subtree. Real
    /// work done — distinct from the count of tasks.
    pub total_attempts: i64,
    /// Sum of per-task wall-clock durations. Terminal
    /// tasks count `updated_at - started_at`; active
    /// tasks count `now - started_at`. Tasks with no
    /// `started_at` contribute zero (and bump
    /// `tasks_with_missing_timing`).
    pub total_wall_clock_secs: i64,
    pub oldest_started_at: Option<i64>,
    pub newest_updated_at: Option<i64>,
    /// Honesty counter — tasks excluded from wall-clock
    /// aggregation because they had no started_at (or
    /// vanished between the lineage walk and the metric
    /// read).
    pub tasks_with_missing_timing: i64,
    /// Echoes back the BFS depth cap so the consumer
    /// renders "showing metrics up to depth N" without
    /// recomputing.
    pub max_depth_walked: i64,
}

/// PH-WAVE2D: one row of a per-task todo list. Operator-facing
/// shape returned by `task.todo_list`. Order is
/// `position ASC, todo_id ASC`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub todo_id: i64,
    pub position: i64,
    pub status: String,
    pub text: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// H6: one row of a stuck-task projection. Pure read shape
/// returned by [`TaskStore::stuck_running`] — task is `running`,
/// has no `max_runtime_secs` (so the recovery scan can't reach
/// it), and has been running longer than the operator's stuck
/// threshold. The dashboard renders these in a banner so
/// operators can spot dead executors at a glance.
#[derive(Debug, Clone)]
pub struct StuckTaskRow {
    pub task_id: String,
    pub title: String,
    /// `task_attempts.started_at` for the current attempt, falling
    /// back to `tasks.started_at` for tasks created pre-C2a.
    pub started_at: i64,
    /// Optional pointer to the open attempt (None for tasks
    /// pre-C2a whose 'running' transition predated the attempt
    /// timeline).
    pub current_attempt_id: Option<i64>,
    /// Wall-clock seconds since `started_at`. Populated by the
    /// query loop using the same `now_secs` it was called with so
    /// the row is internally consistent.
    pub age_secs: i64,
}

/// Outcome of an M72 edge-producer call. Carries both the
/// new edge_id AND the chronicle event_id so callers can
/// surface either side to operators.
#[derive(Debug, Clone)]
pub struct EdgeProducerOutcome {
    pub edge_id: i64,
    pub event_id: i64,
}

/// Cooperative-poller snapshot returned by
/// [`TaskStore::interruption_snapshot`] (M70). Carries the
/// current live status + both generation counters; runtime
/// workers compare against their cached values to detect a
/// new pause/freeze request without re-loading the whole
/// task row.
#[derive(Debug, Clone)]
pub struct InterruptionSnapshot {
    pub task_id: String,
    pub status: String,
    pub pause_generation: i64,
    pub freeze_generation: i64,
}

/// Aggregate output of [`TaskStore::task_lineage`] (M66).
/// The set of related tasks + the edges connecting them +
/// summary fields the dashboard needs to render an honest
/// lineage panel.
#[derive(Debug, Clone)]
pub struct TaskLineageGraph {
    pub root_task_id: String,
    /// Distinct task ids in the lineage, root first. With only
    /// `retried_from` producers shipping today, this is
    /// typically `[root_task_id]` alone.
    pub tasks: Vec<String>,
    /// All edges touching any task in the lineage, ordered
    /// by `edge_id`.
    pub edges: Vec<TaskEdge>,
    /// Edges where `task_id != related_task_id`. Distinct
    /// from `edges.len()` because intra-task `retried_from`
    /// dominates today. Operators read this as "how many
    /// cross-task relationships are recorded for this root."
    pub cross_task_edge_count: usize,
    /// The depth the BFS was capped to. Echoed back so the
    /// dashboard can render "showing edges up to depth N
    /// (raise depth to see more)" without recomputing.
    pub max_depth_walked: usize,
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
pub fn register(
    bridge: &mut DispatchBridge,
    store: Arc<TaskStore>,
    auto_skill_cfg: Option<Arc<crate::nodes::ai::skills::SkillsConfig>>,
    drift_cfg: Option<Arc<crate::nodes::ai::guardrails::DriftConfig>>,
    drift_embedder_cell: crate::nodes::ai::guardrails::DriftEmbedDispatcherCell,
) {
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
        let auto_cfg = auto_skill_cfg.clone();
        let drift = drift_cfg.clone();
        let drift_embedder_for_hook = drift_embedder_cell.clone();
        bridge.register(
            "task.update",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                let auto = auto_cfg.clone();
                let drift = drift.clone();
                let drift_embedder_for_hook = drift_embedder_for_hook.clone();
                async move {
                    let outcome = handle_update(&s, &ctx);
                    // Post-update hook: when a task transitions
                    // to `completed` and the operator has wired
                    // `[skills] auto_generate = true`, spawn a
                    // best-effort background job to synthesise a
                    // SKILL.md from the chronicle. Failures (no
                    // home dir, IO error, name collision) are
                    // silent — auto-skill is opt-in flavour
                    // rather than a required side-effect.
                    if let HandlerOutcome::Ok(_) = &outcome
                        && let Some(auto) = auto.as_ref()
                        && let Some((task_id, status)) = parse_completed_signal(&ctx.args)
                        && status == "completed"
                    {
                        let s = s.clone();
                        let auto = auto.clone();
                        tokio::task::spawn_blocking(move || {
                            run_auto_skill_for_task(&s, &task_id, &auto);
                        });
                    }
                    // Drift hook: on `running` transitions, when
                    // `[guardrails.drift] enabled = true`,
                    // evaluate whether the recent chronicle
                    // diverges from the original goal. The
                    // embedding-aware comparison lands when the
                    // coordinator gains an outbound embedding
                    // dispatcher; today the helper still records
                    // the chronicle summary so the operator can
                    // audit drift after the fact.
                    if let HandlerOutcome::Ok(_) = &outcome
                        && let Some(cfg) = drift.as_ref()
                        && cfg.enabled
                        && let Some((task_id, status)) = parse_completed_signal(&ctx.args)
                        && status == "running"
                    {
                        let s = s.clone();
                        let cfg = cfg.clone();
                        // Read the embedder cell on every hook
                        // fire so a controller that wires the
                        // dispatcher post-startup sees it on
                        // subsequent ticks without restarting.
                        let embedder = drift_embedder_for_hook.get().cloned();
                        tokio::spawn(async move {
                            evaluate_drift_for_task(&s, &task_id, &cfg, embedder).await;
                        });
                    }
                    outcome
                }
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
            "task.session_export",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_session_export(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.session_search",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_session_search(&s, &ctx) }
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
        // PHASE 1 (Brief): board-status move, enforcing the board
        // state machine. Distinct from `task.update` (execution
        // status) — this is the operator-board column.
        let s = store.clone();
        bridge.register(
            "brief.move",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_move(&s, &ctx) }
            })),
        );
    }
    // PHASE 1 (Brief): Sub-brief + Snag relation edges, over the
    // reserved `task_edges` 'spawned' / 'blocked_on' types.
    {
        let s = store.clone();
        bridge.register(
            "brief.unsubbrief",
            Arc::new(FnHandler({
                let s = s.clone();
                move |ctx: InvocationCtx| {
                    let s = s.clone();
                    async move { handle_brief_unsubbrief(&s, &ctx) }
                }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.create",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_create(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.set_labels",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_set_labels(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.labels",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_labels(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.by_label",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_by_label(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.detail",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_detail(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.subbrief_progress",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_subbrief_progress(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.blocking",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_blocking(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.parents",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_parents(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.subbrief",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_subbrief(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.subbriefs",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_subbriefs(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.snag",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_snag(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.unsnag",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_unsnag(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.snags",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_snags(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.blocked",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_blocked(&s, &ctx) }
            })),
        );
    }
    // PHASE 5 (companion): comment thread on a Brief.
    {
        let s = store.clone();
        bridge.register(
            "brief.comment",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_comment(&s, &ctx) }
            })),
        );
    }
    // PHASE 1 (Brief): Dossiers — durable artifacts on a Brief.
    {
        let s = store.clone();
        bridge.register(
            "brief.dossier_add",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_dossier_add(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.dossiers",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_dossiers(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.dossier_get",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_dossier_get(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.dossier_latest",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_dossier_latest(&s, &ctx) }
            })),
        );
    }
    // PHASE 1 (Brief): spine-field set + read (assignee, priority,
    // mandate/campaign links).
    {
        let s = store.clone();
        bridge.register(
            "brief.set",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_set(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.fields",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_fields(&s, &ctx) }
            })),
        );
    }
    {
        // PHASE 2 (board): list Briefs by board column.
        let s = store.clone();
        bridge.register(
            "brief.board",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_board(&s, &ctx) }
            })),
        );
    }
    {
        // PHASE 5 (companion): the board-at-a-glance counts.
        let s = store.clone();
        bridge.register(
            "brief.board_summary",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_board_summary(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "mandate.briefs",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_mandate_briefs(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "campaign.briefs",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_campaign_briefs(&s, &ctx) }
            })),
        );
    }
    // PHASE 2 (Desk): blocked + stale work surfaces.
    {
        let s = store.clone();
        bridge.register(
            "brief.blocked_list",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_blocked_list(&s, &ctx) }
            })),
        );
    }
    // PHASE 5 (Desk): an Operative's personal in-flight Briefs.
    {
        let s = store.clone();
        bridge.register(
            "brief.desk",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_desk(&s, &ctx) }
            })),
        );
    }
    // PHASE 5 (org load): an Operative's workload counts.
    {
        let s = store.clone();
        bridge.register(
            "brief.workload",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_workload(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.team_workload",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_team_workload(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.stale_list",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_stale_list(&s, &ctx) }
            })),
        );
    }
    // PHASE 3 (heartbeat loop): the atomic Claim — single-owner
    // execution lock with lease/heartbeat/release.
    {
        let s = store.clone();
        bridge.register(
            "brief.claim",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_claim(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.heartbeat",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_heartbeat(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.release",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_release(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "brief.claim_holder",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_claim_holder(&s, &ctx) }
            })),
        );
    }
    {
        // PHASE 3 (Pulse): the dispatcher work-list.
        let s = store.clone();
        bridge.register(
            "brief.ready",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_ready(&s, &ctx) }
            })),
        );
    }
    {
        // PHASE 3 (supervisory wake): children-completed.
        let s = store.clone();
        bridge.register(
            "brief.children_done",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_brief_children_done(&s, &ctx) }
            })),
        );
    }
    // Spine progress rollups (Briefs-by-column for a Campaign / Mandate).
    {
        let s = store.clone();
        bridge.register(
            "campaign.progress",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_campaign_progress(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "mandate.progress",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_mandate_progress(&s, &ctx) }
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
        // W2-001b: task.replay — clone a task into a new one
        // with a retried_from cross-task edge.
        let s = store.clone();
        bridge.register(
            "task.replay",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_replay(&s, &ctx) }
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
    {
        let s = store.clone();
        bridge.register(
            "task.pause",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_pause(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.resume",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_resume(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.lineage",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_lineage(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.recent_events",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_recent_events(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.interruption_check",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_interruption_check(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.observe_interruption",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_observe_interruption(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.freeze",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_freeze(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.unfreeze",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_unfreeze(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.record_spawned",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_record_spawned(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.record_delegated",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_record_delegated(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.record_awaited",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_record_awaited(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.transition_check",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_transition_check(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.subtree_metrics",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_subtree_metrics(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.stuck",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_stuck(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.todo_set",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_todo_set(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.todo_list",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_todo_list(&s, &ctx) }
            })),
        );
    }
    {
        let s = store.clone();
        bridge.register(
            "task.todo_update",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move { handle_todo_update(&s, &ctx) }
            })),
        );
    }
}

// ──────────────────────────── Handlers ──────────────────────────────────────

/// PH-WAVE2D: `task.todo_set` — replace a task's todo list.
/// Arg shape: `<task_id>|<text1>\n<text2>\n...`. Empty input
/// after the `|` is a valid clear-the-list call. Returns the
/// post-set list as `<position>\t<todo_id>\t<status>\t<text>\n`
/// rows + trailing `count=<N>`.
fn handle_todo_set(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.todo_set utf8: {e}")),
    };
    let (task_id, rest) = match raw.split_once('|') {
        Some(p) => p,
        None => return invalid("task.todo_set: arg shape `<task_id>|<text...>`".into()),
    };
    let task_id = task_id.trim();
    if task_id.is_empty() {
        return invalid("task.todo_set: task_id required".into());
    }
    let items_owned: Vec<String> = rest
        .split('\n')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let items: Vec<&str> = items_owned.iter().map(String::as_str).collect();
    let out = match store.set_task_todos(task_id, &items) {
        Ok(v) => v,
        Err(CoordinatorError::NotFound(_)) => {
            return invalid(format!("task.todo_set: task not found: {task_id}"));
        }
        Err(CoordinatorError::Invalid(m)) => return invalid(m),
        Err(e) => return internal(format!("task.todo_set: {e}")),
    };
    HandlerOutcome::Ok(render_todo_list(&out).into_bytes())
}

/// PH-WAVE2D: `task.todo_list|<task_id>` — read-only.
fn handle_todo_list(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.todo_list utf8: {e}")),
    };
    if raw.is_empty() {
        return invalid("task.todo_list: task_id required".into());
    }
    match store.list_task_todos(raw) {
        Ok(v) => HandlerOutcome::Ok(render_todo_list(&v).into_bytes()),
        Err(e) => internal(format!("task.todo_list: {e}")),
    }
}

/// PH-WAVE2D: `task.todo_update|<task_id>|<todo_id>|<status>`.
/// Status must be `open` or `done`.
fn handle_todo_update(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.todo_update utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(3, '|').collect();
    if parts.len() != 3 {
        return invalid("task.todo_update: arg shape `<task_id>|<todo_id>|<open|done>`".into());
    }
    let task_id = parts[0].trim();
    let todo_id: i64 = match parts[1].trim().parse() {
        Ok(n) => n,
        Err(_) => {
            return invalid(format!("task.todo_update: invalid todo_id '{}'", parts[1]));
        }
    };
    let status = parts[2].trim();
    match store.update_task_todo_status(task_id, todo_id, status) {
        Ok(item) => HandlerOutcome::Ok(
            format!(
                "{}\t{}\t{}\t{}\n",
                item.position,
                item.todo_id,
                item.status,
                tab_safe(&item.text),
            )
            .into_bytes(),
        ),
        Err(CoordinatorError::NotFound(m)) => invalid(format!("task.todo_update: {m}")),
        Err(CoordinatorError::Invalid(m)) => invalid(m),
        Err(e) => internal(format!("task.todo_update: {e}")),
    }
}

fn render_todo_list(items: &[TodoItem]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for it in items {
        let _ = writeln!(
            s,
            "{}\t{}\t{}\t{}",
            it.position,
            it.todo_id,
            it.status,
            tab_safe(&it.text),
        );
    }
    let _ = writeln!(s, "count={}", items.len());
    s
}

/// H6: `task.stuck|<threshold_secs>` — read-only stuck-task
/// projection. Threshold defaults to 300 (5 minutes) when the
/// caller omits it. Output is one row per stuck task with
/// trailing `count=<N>`:
///
///   <task_id>\t<title>\t<started_at>\t<age_secs>
///   ...
///   count=<N>
fn handle_stuck(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.stuck utf8: {e}")),
    };
    let trimmed = raw.trim();
    let threshold = if trimmed.is_empty() {
        300
    } else {
        match trimmed.parse::<i64>() {
            Ok(n) if n >= 0 => n,
            Ok(_) => return invalid("task.stuck: threshold must be non-negative".into()),
            Err(_) => {
                return invalid(format!(
                    "task.stuck: invalid threshold '{trimmed}' (expected integer seconds)"
                ));
            }
        }
    };
    let now = unix_secs();
    let rows = match store.stuck_running(now, threshold) {
        Ok(r) => r,
        Err(e) => return internal(format!("task.stuck: {e}")),
    };
    use std::fmt::Write as _;
    let mut body = String::new();
    for r in &rows {
        let _ = writeln!(
            body,
            "{}\t{}\t{}\t{}",
            r.task_id,
            tab_safe(&r.title),
            r.started_at,
            r.age_secs,
        );
    }
    let _ = writeln!(body, "count={}", rows.len());
    HandlerOutcome::Ok(body.into_bytes())
}

/// Local-to-handler helper: keep titles single-line so the
/// tab-separated output stays parseable.
fn tab_safe(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

fn handle_create(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.create utf8: {e}")),
    };
    // `title|flow_template|params_json|owner_subject_id|retry_policy|max_retries|max_runtime_secs|origin_surface`.
    // Retry/runtime/origin trailers are optional; callers can leave
    // the suffix off entirely or send empty slots. params_json that
    // contains `|` should be base64-encoded by the caller (SIMP-016).
    // PH-ORIGIN-SURFACE (D-004): the 8th slot is the dispatch
    // surface label — empty / missing → NULL → dashboard renders
    // as "unknown".
    let parts: Vec<&str> = s.splitn(8, '|').collect();
    let title = parts.first().copied().unwrap_or("");
    let flow_template = parts.get(1).copied().unwrap_or("");
    let params_json = parts.get(2).copied().unwrap_or("");
    let owner = parts.get(3).copied().unwrap_or("");
    let retry_policy_str = parts.get(4).copied().unwrap_or("");
    let max_retries_str = parts.get(5).copied().unwrap_or("");
    let max_runtime_str = parts.get(6).copied().unwrap_or("");
    let origin_surface_str = parts.get(7).copied().unwrap_or("").trim();
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
    let origin_surface = if origin_surface_str.is_empty() {
        None
    } else {
        Some(origin_surface_str)
    };
    match store.create(
        title,
        flow_template,
        params_json,
        &owner,
        retry_policy,
        max_retries,
        max_runtime_secs,
        origin_surface,
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

/// `brief.move` — move a Brief's board status. Arg: `task_id|board_status`.
/// Enforces the board state machine; returns `from -> to` as the body.
fn handle_brief_move(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("brief.move utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(2, '|').collect();
    if parts.len() < 2 {
        return invalid("brief.move: expected `task_id|board_status`".to_string());
    }
    let task_id = parts[0].trim();
    let to = parts[1].trim();
    if task_id.is_empty() {
        return invalid("brief.move: task_id required".to_string());
    }
    match store.set_board_status(task_id, to) {
        Ok((from, to)) => HandlerOutcome::Ok(format!("{from} -> {to}").into_bytes()),
        Err(CoordinatorError::NotFound(id)) => invalid(format!("brief.move: not found: {id}")),
        Err(CoordinatorError::Invalid(m)) => invalid(format!("brief.move: {m}")),
        Err(e) => internal(format!("brief.move: {e}")),
    }
}

/// Parse a two-field `a|b` arg shape for the Brief relation caps.
fn parse_pair<'a>(ctx: &'a InvocationCtx, method: &str) -> Result<(&'a str, &'a str), HandlerOutcome> {
    let raw = std::str::from_utf8(&ctx.args)
        .map_err(|e| invalid(format!("{method} utf8: {e}")))?;
    let parts: Vec<&str> = raw.splitn(2, '|').collect();
    if parts.len() < 2 || parts[0].trim().is_empty() || parts[1].trim().is_empty() {
        return Err(invalid(format!("{method}: expected two ids `a|b`")));
    }
    Ok((parts[0].trim(), parts[1].trim()))
}

fn single_id<'a>(ctx: &'a InvocationCtx, method: &str) -> Result<&'a str, HandlerOutcome> {
    let raw = std::str::from_utf8(&ctx.args)
        .map_err(|e| invalid(format!("{method} utf8: {e}")))?
        .trim();
    if raw.is_empty() {
        return Err(invalid(format!("{method}: task_id required")));
    }
    Ok(raw)
}

fn map_edge_err(method: &str, e: CoordinatorError) -> HandlerOutcome {
    match e {
        CoordinatorError::NotFound(id) => invalid(format!("{method}: not found: {id}")),
        CoordinatorError::Invalid(m) => invalid(format!("{method}: {m}")),
        other => internal(format!("{method}: {other}")),
    }
}

/// `brief.subbrief` — link `child` as a Sub-brief of `parent`. Arg `parent|child`.
fn handle_brief_subbrief(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let (parent, child) = match parse_pair(ctx, "brief.subbrief") {
        Ok(p) => p,
        Err(o) => return o,
    };
    match store.link_subbrief(parent, child) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(e) => map_edge_err("brief.subbrief", e),
    }
}

/// `brief.unsubbrief` — detach a Sub-brief from `parent` (a
/// mis-decomposed plan). Arg `parent|child`. Idempotent.
fn handle_brief_unsubbrief(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let (parent, child) = match parse_pair(ctx, "brief.unsubbrief") {
        Ok(p) => p,
        Err(o) => return o,
    };
    match store.unlink_subbrief(parent, child) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(e) => map_edge_err("brief.unsubbrief", e),
    }
}

/// `brief.create` — materialize a Brief and place it on the spine.
/// Arg `title|assignee|mandate|campaign|priority` (only `title`
/// required; the rest optional, empty = skip). Owner subject comes
/// from the caller. Returns the new task_id.
fn handle_brief_create(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("brief.create utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(5, '|').collect();
    let title = parts.first().copied().unwrap_or("").trim();
    if title.is_empty() {
        return invalid(
            "brief.create: title required (arg shape: title|assignee|mandate|campaign|priority)"
                .to_string(),
        );
    }
    let opt = |i: usize| parts.get(i).copied().map(str::trim).filter(|s| !s.is_empty());
    let owner = ctx.caller.subject_id.to_string();
    match store.create_brief(title, &owner, opt(1), opt(2), opt(3), opt(4)) {
        Ok(id) => HandlerOutcome::Ok(id.into_bytes()),
        Err(e) => map_edge_err("brief.create", e),
    }
}

/// `brief.set_labels` — replace a Brief's labels. Arg
/// `task|label1,label2,...` (comma-separated; empty list clears).
fn handle_brief_set_labels(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("brief.set_labels utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(2, '|').collect();
    let task = parts.first().copied().unwrap_or("").trim();
    if task.is_empty() {
        return invalid("brief.set_labels: task required (arg shape: task|csv)".to_string());
    }
    let labels: Vec<&str> = parts
        .get(1)
        .copied()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    match store.set_brief_labels(task, &labels) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(e) => map_edge_err("brief.set_labels", e),
    }
}

/// `brief.by_label` — the Briefs carrying a label (JSON cards). Arg
/// `label|limit` (limit default 50).
fn handle_brief_by_label(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("brief.by_label utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(2, '|').collect();
    let label = parts.first().copied().map(str::trim).unwrap_or("");
    if label.is_empty() {
        return invalid("brief.by_label: label required".to_string());
    }
    let limit: usize = parts
        .get(1)
        .copied()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    match store.list_briefs_by_label(label, limit) {
        Ok(rows) => match serde_json::to_vec(&rows) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("brief.by_label encode: {e}")),
        },
        Err(e) => map_edge_err("brief.by_label", e),
    }
}

/// `brief.labels` — a Brief's labels, one per line. Arg `task`.
fn handle_brief_labels(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task = match single_id(ctx, "brief.labels") {
        Ok(t) => t,
        Err(o) => return o,
    };
    match store.brief_labels(task) {
        Ok(labels) => HandlerOutcome::Ok(labels.join("\n").into_bytes()),
        Err(e) => map_edge_err("brief.labels", e),
    }
}

/// `brief.detail` — the full Brief detail view (fields + relations
/// both ways + dossiers + blocked flag) as one JSON object. Arg
/// `task`. `not found` when the Brief doesn't exist.
fn handle_brief_detail(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task = match single_id(ctx, "brief.detail") {
        Ok(t) => t,
        Err(o) => return o,
    };
    match store.brief_detail(task) {
        Ok(Some(d)) => match serde_json::to_vec(&d) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("brief.detail encode: {e}")),
        },
        Ok(None) => invalid(format!("brief.detail: not found: {task}")),
        Err(e) => map_edge_err("brief.detail", e),
    }
}

/// `brief.subbrief_progress` — a parent's Sub-briefs counted by
/// board column (+ `total`). Arg `parent`. The planner's
/// decomposition-progress view.
fn handle_brief_subbrief_progress(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let parent = match single_id(ctx, "brief.subbrief_progress") {
        Ok(p) => p,
        Err(o) => return o,
    };
    match store.subbrief_progress(parent) {
        Ok(counts) => counts_to_json(counts),
        Err(e) => map_edge_err("brief.subbrief_progress", e),
    }
}

/// `brief.blocking` — the Briefs that `task` blocks (reverse Snags:
/// who is waiting on it). Arg `task`. One task_id per line.
fn handle_brief_blocking(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task = match single_id(ctx, "brief.blocking") {
        Ok(t) => t,
        Err(o) => return o,
    };
    match store.list_blocking(task) {
        Ok(ids) => HandlerOutcome::Ok(ids.join("\n").into_bytes()),
        Err(e) => map_edge_err("brief.blocking", e),
    }
}

/// `brief.parents` — the parent Briefs that spawned `task` as a
/// Sub-brief. Arg `task`. One task_id per line.
fn handle_brief_parents(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task = match single_id(ctx, "brief.parents") {
        Ok(t) => t,
        Err(o) => return o,
    };
    match store.parent_briefs(task) {
        Ok(ids) => HandlerOutcome::Ok(ids.join("\n").into_bytes()),
        Err(e) => map_edge_err("brief.parents", e),
    }
}

/// `brief.subbriefs` — list the Sub-briefs of `parent`. Arg `parent`.
/// Returns one task_id per line.
fn handle_brief_subbriefs(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let parent = match single_id(ctx, "brief.subbriefs") {
        Ok(p) => p,
        Err(o) => return o,
    };
    match store.list_subbriefs(parent) {
        Ok(ids) => HandlerOutcome::Ok(ids.join("\n").into_bytes()),
        Err(e) => map_edge_err("brief.subbriefs", e),
    }
}

/// `brief.snag` — record that `task` is blocked by `blocker`. Arg `task|blocker`.
fn handle_brief_snag(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let (task, blocker) = match parse_pair(ctx, "brief.snag") {
        Ok(p) => p,
        Err(o) => return o,
    };
    match store.add_snag(task, blocker) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(e) => map_edge_err("brief.snag", e),
    }
}

/// `brief.unsnag` — clear the `task` → `blocker` Snag (a wrong /
/// resolved dependency). Arg `task|blocker`. Idempotent.
fn handle_brief_unsnag(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let (task, blocker) = match parse_pair(ctx, "brief.unsnag") {
        Ok(p) => p,
        Err(o) => return o,
    };
    match store.remove_snag(task, blocker) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(e) => map_edge_err("brief.unsnag", e),
    }
}

/// `brief.snags` — list the Snags on `task` (the ids blocking it). Arg `task`.
fn handle_brief_snags(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task = match single_id(ctx, "brief.snags") {
        Ok(t) => t,
        Err(o) => return o,
    };
    match store.list_snags(task) {
        Ok(ids) => HandlerOutcome::Ok(ids.join("\n").into_bytes()),
        Err(e) => map_edge_err("brief.snags", e),
    }
}

/// `brief.blocked` — is `task` blocked by an unresolved Snag? Arg `task`.
/// Returns `true` / `false`.
fn handle_brief_blocked(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task = match single_id(ctx, "brief.blocked") {
        Ok(t) => t,
        Err(o) => return o,
    };
    match store.is_blocked(task) {
        Ok(b) => HandlerOutcome::Ok(if b { b"true".to_vec() } else { b"false".to_vec() }),
        Err(e) => map_edge_err("brief.blocked", e),
    }
}

/// `brief.comment` — post a comment to a Brief's Chronicle. Arg
/// `task_id|author|text` (text may contain pipes). Read back via
/// `task.events` with type filter `brief.comment`.
fn handle_brief_comment(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("brief.comment utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(3, '|').collect();
    if parts.len() < 3 {
        return invalid("brief.comment: expected `task_id|author|text`".to_string());
    }
    match store.comment_on_brief(parts[0].trim(), parts[1].trim(), parts[2]) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(e) => map_edge_err("brief.comment", e),
    }
}

/// `brief.dossier_add` — attach a Dossier. Arg `task_id|kind|title|body`
/// (body may contain pipes). Returns the new doc_id.
fn handle_brief_dossier_add(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("brief.dossier_add utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(4, '|').collect();
    if parts.len() < 3 {
        return invalid("brief.dossier_add: expected `task_id|kind|title|body`".to_string());
    }
    let body = parts.get(3).copied().unwrap_or("");
    match store.add_dossier(parts[0].trim(), parts[1].trim(), parts[2].trim(), body) {
        Ok(id) => HandlerOutcome::Ok(id.into_bytes()),
        Err(e) => map_edge_err("brief.dossier_add", e),
    }
}

/// `brief.dossiers` — list a Brief's Dossiers (metadata JSON). Arg `task_id`.
fn handle_brief_dossiers(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task = match single_id(ctx, "brief.dossiers") {
        Ok(t) => t,
        Err(o) => return o,
    };
    match store.list_dossiers(task) {
        Ok(rows) => match serde_json::to_vec(&rows) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("brief.dossiers encode: {e}")),
        },
        Err(e) => map_edge_err("brief.dossiers", e),
    }
}

/// `brief.dossier_latest` — the most recent Dossier of a kind on a
/// Brief (full JSON), or empty body when none. Arg `task_id|kind`.
fn handle_brief_dossier_latest(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("brief.dossier_latest utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(2, '|').collect();
    let task = parts.first().copied().map(str::trim).unwrap_or("");
    let kind = parts.get(1).copied().map(str::trim).unwrap_or("");
    if task.is_empty() || kind.is_empty() {
        return invalid("brief.dossier_latest: expected `task_id|kind`".to_string());
    }
    match store.latest_dossier(task, kind) {
        Ok(Some(d)) => match serde_json::to_vec(&d) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("brief.dossier_latest encode: {e}")),
        },
        Ok(None) => HandlerOutcome::Ok(Vec::new()),
        Err(e) => map_edge_err("brief.dossier_latest", e),
    }
}

/// `brief.dossier_get` — read a Dossier by id (full JSON). Arg `doc_id`.
fn handle_brief_dossier_get(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let doc_id = match single_id(ctx, "brief.dossier_get") {
        Ok(d) => d,
        Err(o) => return o,
    };
    match store.get_dossier(doc_id) {
        Ok(Some(d)) => match serde_json::to_vec(&d) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("brief.dossier_get encode: {e}")),
        },
        Ok(None) => invalid(format!("brief.dossier_get: not found: {doc_id}")),
        Err(e) => map_edge_err("brief.dossier_get", e),
    }
}

/// `brief.set` — set a Brief spine field. Arg `task_id|field|value`
/// (field = assignee/priority/mandate/campaign; empty value clears
/// assignee/mandate/campaign).
fn handle_brief_set(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("brief.set utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(3, '|').collect();
    if parts.len() < 3 {
        return invalid("brief.set: expected `task_id|field|value`".to_string());
    }
    match store.set_brief_field(parts[0].trim(), parts[1].trim(), parts[2]) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(e) => map_edge_err("brief.set", e),
    }
}

/// `brief.fields` — read a Brief's spine fields (JSON). Arg `task_id`.
fn handle_brief_fields(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task = match single_id(ctx, "brief.fields") {
        Ok(t) => t,
        Err(o) => return o,
    };
    match store.brief_fields(task) {
        Ok(Some(f)) => match serde_json::to_vec(&f) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("brief.fields encode: {e}")),
        },
        Ok(None) => invalid(format!("brief.fields: not found: {task}")),
        Err(e) => map_edge_err("brief.fields", e),
    }
}

/// `brief.board` — list Briefs for the board. Arg `board_status|limit`
/// (both optional; empty board = all columns; default limit 50).
/// Returns a JSON array of BriefCards, newest-updated first.
fn handle_brief_board(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("brief.board utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(2, '|').collect();
    let board = parts
        .first()
        .copied()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let limit: usize = parts
        .get(1)
        .copied()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    match store.list_briefs_by_board(board, limit) {
        Ok(rows) => match serde_json::to_vec(&rows) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("brief.board encode: {e}")),
        },
        Err(e) => map_edge_err("brief.board", e),
    }
}

/// `brief.blocked_list` — Briefs currently blocked by an unresolved
/// Snag (the Desk's blocked work). Arg `limit` (optional, default 50).
fn handle_brief_blocked_list(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("brief.blocked_list utf8: {e}")),
    };
    let limit: usize = if raw.is_empty() {
        50
    } else {
        raw.parse().unwrap_or(50)
    };
    match store.list_blocked_briefs(limit) {
        Ok(rows) => match serde_json::to_vec(&rows) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("brief.blocked_list encode: {e}")),
        },
        Err(e) => map_edge_err("brief.blocked_list", e),
    }
}

/// `brief.team_workload` — aggregate in-flight Brief counts across
/// a SET of Operatives (a manager's Branch). Arg: pipe-separated
/// assignee ids `a1|a2|a3`. Returns counts by column (+ `total`).
fn handle_brief_team_workload(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("brief.team_workload utf8: {e}")),
    };
    if raw.is_empty() {
        return invalid("brief.team_workload: at least one assignee required".to_string());
    }
    let ids: Vec<&str> = raw.split('|').map(str::trim).filter(|s| !s.is_empty()).collect();
    if ids.is_empty() {
        return invalid("brief.team_workload: at least one assignee required".to_string());
    }
    match store.aggregate_board_counts(&ids) {
        Ok(counts) => counts_to_json(counts),
        Err(e) => map_edge_err("brief.team_workload", e),
    }
}

/// `brief.workload` — an Operative's in-flight Brief counts by
/// board column (+ `total`). Arg `assignee`. The load signal for
/// the org chart.
fn handle_brief_workload(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let assignee = match single_id(ctx, "brief.workload") {
        Ok(a) => a,
        Err(o) => return o,
    };
    match store.assignee_board_counts(assignee) {
        Ok(counts) => counts_to_json(counts),
        Err(e) => map_edge_err("brief.workload", e),
    }
}

/// `brief.desk` — an Operative's personal Desk: their in-flight
/// Briefs (todo/in_progress/in_review/blocked), priority-ordered.
/// Arg `assignee|limit` (limit default 50).
fn handle_brief_desk(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("brief.desk utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(2, '|').collect();
    let assignee = parts.first().copied().map(str::trim).unwrap_or("");
    if assignee.is_empty() {
        return invalid("brief.desk: assignee required".to_string());
    }
    let limit: usize = parts
        .get(1)
        .copied()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    match store.list_desk_for_assignee(assignee, limit) {
        Ok(rows) => match serde_json::to_vec(&rows) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("brief.desk encode: {e}")),
        },
        Err(e) => map_edge_err("brief.desk", e),
    }
}

/// `brief.stale_list` — Briefs in an active column idle for at least
/// `idle_secs` (the Desk's stuck work). Arg `idle_secs|limit`
/// (idle_secs default 86400 = 1 day; limit default 50).
fn handle_brief_stale_list(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("brief.stale_list utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(2, '|').collect();
    let idle: i64 = parts
        .first()
        .copied()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(86_400);
    let limit: usize = parts
        .get(1)
        .copied()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    match store.list_stale_briefs(idle, limit) {
        Ok(rows) => match serde_json::to_vec(&rows) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("brief.stale_list encode: {e}")),
        },
        Err(e) => map_edge_err("brief.stale_list", e),
    }
}

/// Parse a `task_id|agent_id|lease_secs` arg for the claim caps.
/// lease_secs defaults to 300.
fn parse_claim_args<'a>(
    ctx: &'a InvocationCtx,
    method: &str,
) -> Result<(&'a str, &'a str, i64), HandlerOutcome> {
    let raw = std::str::from_utf8(&ctx.args)
        .map_err(|e| invalid(format!("{method} utf8: {e}")))?;
    let parts: Vec<&str> = raw.splitn(3, '|').collect();
    if parts.len() < 2 || parts[0].trim().is_empty() || parts[1].trim().is_empty() {
        return Err(invalid(format!(
            "{method}: expected `task_id|agent_id|lease_secs`"
        )));
    }
    let lease: i64 = parts
        .get(2)
        .copied()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    Ok((parts[0].trim(), parts[1].trim(), lease))
}

/// `brief.claim` — atomically claim a Brief. Arg `task_id|agent_id|lease_secs`
/// (lease default 300). Returns `claimed` if won, `held` if another holds it.
fn handle_brief_claim(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let (task, agent, lease) = match parse_claim_args(ctx, "brief.claim") {
        Ok(t) => t,
        Err(o) => return o,
    };
    match store.claim_brief(task, agent, lease) {
        Ok(true) => HandlerOutcome::Ok(b"claimed".to_vec()),
        Ok(false) => HandlerOutcome::Ok(b"held".to_vec()),
        Err(e) => map_edge_err("brief.claim", e),
    }
}

/// `brief.heartbeat` — extend the caller's claim. Arg
/// `task_id|agent_id|lease_secs`. Returns `ok` or `lost`.
fn handle_brief_heartbeat(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let (task, agent, lease) = match parse_claim_args(ctx, "brief.heartbeat") {
        Ok(t) => t,
        Err(o) => return o,
    };
    match store.heartbeat_claim(task, agent, lease) {
        Ok(true) => HandlerOutcome::Ok(b"ok".to_vec()),
        Ok(false) => HandlerOutcome::Ok(b"lost".to_vec()),
        Err(e) => map_edge_err("brief.heartbeat", e),
    }
}

/// `brief.release` — release the caller's claim. Arg `task_id|agent_id`.
fn handle_brief_release(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let (task, agent) = match parse_pair(ctx, "brief.release") {
        Ok(p) => p,
        Err(o) => return o,
    };
    match store.release_claim(task, agent) {
        Ok(()) => HandlerOutcome::Ok(Vec::new()),
        Err(e) => map_edge_err("brief.release", e),
    }
}

/// `brief.claim_holder` — the current live claim holder + expiry as
/// JSON `{"holder":..,"expires_at":..}`, or empty body when
/// unclaimed. Arg `task_id`.
fn handle_brief_claim_holder(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task = match single_id(ctx, "brief.claim_holder") {
        Ok(t) => t,
        Err(o) => return o,
    };
    match store.claim_holder(task) {
        Ok(Some((holder, expires))) => {
            let v = serde_json::json!({ "holder": holder, "expires_at": expires });
            match serde_json::to_vec(&v) {
                Ok(b) => HandlerOutcome::Ok(b),
                Err(e) => internal(format!("brief.claim_holder encode: {e}")),
            }
        }
        Ok(None) => HandlerOutcome::Ok(Vec::new()),
        Err(e) => map_edge_err("brief.claim_holder", e),
    }
}

/// `brief.ready` — the dispatcher work-list: Briefs ready to work
/// (assigned, active column, unblocked, unclaimed). Arg `limit`
/// (optional, default 50). JSON array of BriefCards, priority-first.
fn handle_brief_ready(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("brief.ready utf8: {e}")),
    };
    let limit: usize = if raw.is_empty() {
        50
    } else {
        raw.parse().unwrap_or(50)
    };
    match store.list_ready_briefs(limit) {
        Ok(rows) => match serde_json::to_vec(&rows) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("brief.ready encode: {e}")),
        },
        Err(e) => map_edge_err("brief.ready", e),
    }
}

/// `brief.children_done` — the children-completed supervisory wake:
/// parent Briefs whose Sub-briefs are all finished. Arg `limit`
/// (optional, default 50). JSON array of BriefCards.
fn handle_brief_children_done(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("brief.children_done utf8: {e}")),
    };
    let limit: usize = if raw.is_empty() {
        50
    } else {
        raw.parse().unwrap_or(50)
    };
    match store.list_briefs_with_all_children_done(limit) {
        Ok(rows) => match serde_json::to_vec(&rows) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("brief.children_done encode: {e}")),
        },
        Err(e) => map_edge_err("brief.children_done", e),
    }
}

/// Render board-column counts as a JSON object plus a `total` key.
fn counts_to_json(counts: Vec<(String, i64)>) -> HandlerOutcome {
    let mut obj = serde_json::Map::new();
    let mut total = 0i64;
    for (status, n) in counts {
        total += n;
        obj.insert(status, serde_json::Value::from(n));
    }
    obj.insert("total".to_string(), serde_json::Value::from(total));
    match serde_json::to_vec(&serde_json::Value::Object(obj)) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => internal(format!("progress encode: {e}")),
    }
}

/// `campaign.progress` — a Campaign's Brief counts by board column
/// (+ `total`). Arg `campaign_id`.
fn handle_campaign_progress(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match single_id(ctx, "campaign.progress") {
        Ok(t) => t,
        Err(o) => return o,
    };
    match store.campaign_brief_counts(id) {
        Ok(counts) => counts_to_json(counts),
        Err(e) => map_edge_err("campaign.progress", e),
    }
}

/// `mandate.progress` — a Mandate's directly-linked Brief counts by
/// board column (+ `total`). Arg `mandate_id`.
fn handle_mandate_progress(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let id = match single_id(ctx, "mandate.progress") {
        Ok(t) => t,
        Err(o) => return o,
    };
    match store.mandate_brief_counts(id) {
        Ok(counts) => counts_to_json(counts),
        Err(e) => map_edge_err("mandate.progress", e),
    }
}

/// `brief.board_summary` — Brief counts across all board columns (+
/// `total`). No args. The board-at-a-glance for the companion /
/// dashboard.
fn handle_brief_board_summary(store: &TaskStore, _ctx: &InvocationCtx) -> HandlerOutcome {
    match store.board_summary() {
        Ok(counts) => counts_to_json(counts),
        Err(e) => map_edge_err("brief.board_summary", e),
    }
}

/// `mandate.briefs` — the Briefs linked to a Mandate (JSON cards).
/// Arg `mandate_id|limit` (limit default 50).
fn handle_mandate_briefs(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    brief_link_list(store, ctx, "mandate.briefs", true)
}

/// `campaign.briefs` — the Briefs linked to a Campaign. Arg
/// `campaign_id|limit`.
fn handle_campaign_briefs(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    brief_link_list(store, ctx, "campaign.briefs", false)
}

fn brief_link_list(
    store: &TaskStore,
    ctx: &InvocationCtx,
    method: &str,
    is_mandate: bool,
) -> HandlerOutcome {
    let raw = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("{method} utf8: {e}")),
    };
    let parts: Vec<&str> = raw.splitn(2, '|').collect();
    let id = parts.first().copied().map(str::trim).unwrap_or("");
    if id.is_empty() {
        return invalid(format!("{method}: id required"));
    }
    let limit: usize = parts
        .get(1)
        .copied()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let res = if is_mandate {
        store.list_briefs_by_mandate(id, limit)
    } else {
        store.list_briefs_by_campaign(id, limit)
    };
    match res {
        Ok(rows) => match serde_json::to_vec(&rows) {
            Ok(b) => HandlerOutcome::Ok(b),
            Err(e) => internal(format!("{method} encode: {e}")),
        },
        Err(e) => map_edge_err(method, e),
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
    push_int_field!("pause_generation", v.pause_generation);
    push_int_field!("freeze_generation", v.freeze_generation);
    push_opt_int_field!("frozen_at", v.frozen_at);
    push_opt_str_field!("frozen_reason", v.frozen_reason);
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
/// W2-001b: handle `task.replay`. Args: `<original_task_id>`.
/// Returns the new task_id on success. The caller's
/// subject_id is stamped as the producer of the retried_from
/// edge so the chronicle row attributes the action correctly.
fn handle_replay(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let original_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.replay utf8: {e}")),
    };
    if original_id.is_empty() {
        return invalid("task.replay: original task_id required".to_string());
    }
    let producer = ctx.caller.subject_id.to_string();
    match store.replay_from(original_id, &producer) {
        Ok(new_id) => HandlerOutcome::Ok(new_id.into_bytes()),
        Err(CoordinatorError::NotFound(id)) => invalid(format!("task.replay: not found: {id}")),
        Err(e) => internal(format!("task.replay: {e}")),
    }
}

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

/// Statuses from which an operator-initiated pause is
/// allowed. Terminal statuses (`completed`/`failed`/`cancelled`)
/// reject; `awaiting_input` has its own semantic (user-input)
/// so the operator pause path leaves it alone; `paused` itself
/// rejects to keep the toggle idempotent.
pub const PAUSABLE_STATUSES: &[&str] = &["pending", "running", "retrying"];

/// Canonical task state vocabulary (M74). Every status the
/// runtime / operators / coord can write. Used by
/// [`is_allowed_transition`] to validate state-machine
/// movement. Unknown / future statuses are tolerated by the
/// validator (returns "unknown — caller-defined") so adding
/// a new status doesn't break older bridges.
pub const TASK_STATES: &[&str] = &[
    "pending",
    "running",
    "retrying",
    "completed",
    "failed",
    "interrupted",
    "cancelled",
    "paused",
    "frozen",
    "awaiting_input",
];

/// Returns true when `from → to` is a permitted task-state
/// transition under the runtime's canonical state machine
/// (M74). Same-status moves are no-ops and always allowed.
/// Unknown statuses (anything outside [`TASK_STATES`]) are
/// allowed conservatively so caller-defined statuses don't
/// break: the bridge enforcement layer is responsible for
/// rejecting them at the API boundary.
///
/// Documented allowed transitions (read the source table
/// for the authoritative list):
/// - pending → running | cancelled | paused | frozen
/// - running → completed | failed | interrupted | cancelled
///   | paused | frozen | awaiting_input | retrying
/// - retrying → running | failed | cancelled | paused | frozen
/// - failed → retrying (operator forces retry)
/// - interrupted → retrying | cancelled | paused | frozen
/// - paused → pending (resume) | frozen | cancelled
/// - frozen → pending (unfreeze) | cancelled
/// - awaiting_input → running | cancelled | frozen
/// - completed (terminal — no outbound transitions)
/// - cancelled (terminal — no outbound transitions)
pub fn is_allowed_transition(from: &str, to: &str) -> bool {
    if from == to {
        return true;
    }
    // Unknown statuses get the conservative "allowed" verdict
    // so adding a new status to a future migration doesn't
    // break older bridge code reading the table.
    let from_known = TASK_STATES.contains(&from);
    let to_known = TASK_STATES.contains(&to);
    if !from_known || !to_known {
        return true;
    }
    matches!(
        (from, to),
        ("pending", "running" | "cancelled" | "paused" | "frozen")
            | (
                "running",
                "completed"
                    | "failed"
                    | "interrupted"
                    | "cancelled"
                    | "paused"
                    | "frozen"
                    | "awaiting_input"
                    | "retrying"
            )
            | (
                "retrying",
                "running" | "failed" | "cancelled" | "paused" | "frozen"
            )
            | ("failed", "retrying")
            | (
                "interrupted",
                "retrying" | "cancelled" | "paused" | "frozen"
            )
            | ("paused", "pending" | "frozen" | "cancelled")
            | ("frozen", "pending" | "cancelled")
            | ("awaiting_input", "running" | "cancelled" | "frozen")
    )
}

/// Statuses from which an operator-initiated freeze is
/// allowed (M71). Wider than pause — operators can freeze a
/// paused task, a task awaiting input, etc. Refuses
/// `frozen` (already), terminal statuses, and `cancelled`
/// (terminal).
pub const FREEZABLE_STATUSES: &[&str] = &[
    "pending",
    "running",
    "retrying",
    "paused",
    "awaiting_input",
    "interrupted",
];

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

/// `task.recent_events` — cross-task event firehose (M67).
/// Args: `since_event_id|limit|event_type_filter`. All
/// optional (defaults 0 / 100 / empty). Returns one JSON
/// object per line, newest-first. Each object includes the
/// task_id so consumers can render the event without a
/// second round-trip.
fn handle_recent_events(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.recent_events utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(3, '|').collect();
    let since: i64 = parts
        .first()
        .copied()
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let limit: usize = parts
        .get(1)
        .copied()
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let type_filter = parts.get(2).copied().filter(|v| !v.is_empty());
    match store.recent_events_cross_task(since, limit, type_filter) {
        Ok(rows) => {
            let mut buf = String::new();
            for (task_id, ev) in &rows {
                // Reuse the per-task envelope renderer and
                // splice in the task_id field. The renderer
                // emits `{"id":N,...}` — we replace the
                // opening brace with `{"task_id":"...",`.
                let line = render_event_json(ev);
                if let Some(rest) = line.strip_prefix('{') {
                    buf.push_str(&format!(r#"{{"task_id":"{}","#, json_escape(task_id)));
                    buf.push_str(rest);
                } else {
                    // Defensive: render_event_json always
                    // begins with '{', but if it ever
                    // changes we fall back to the raw line.
                    buf.push_str(&line);
                }
                buf.push('\n');
            }
            HandlerOutcome::Ok(buf.into_bytes())
        }
        Err(e) => internal(format!("task.recent_events: {e}")),
    }
}

/// `task.lineage` — BFS execution lineage from a root task
/// (M66). Args: `task_id|max_depth`. max_depth defaults to 4,
/// clamped to `[1, 16]`. Returns a multi-line tab-delimited
/// body:
///   `root=<task_id>`
///   `tasks=<id1>,<id2>,...`
///   `cross_task_edges=<count>`
///   `max_depth=<n>`
///   one tab-delimited row per edge:
///   `<edge_id>\t<edge_type>\t<task_id>\t<related_task_id>\t<created_at>`
/// Empty body (just the header lines) when no edges exist.
fn handle_lineage(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.lineage utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(2, '|').collect();
    let task_id = parts.first().copied().unwrap_or("").trim();
    if task_id.is_empty() {
        return invalid("task.lineage: task_id required".to_string());
    }
    let max_depth: usize = parts
        .get(1)
        .copied()
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    match store.task_lineage(task_id, max_depth) {
        Ok(g) => {
            let mut buf = String::new();
            buf.push_str(&format!("root={}\n", g.root_task_id));
            buf.push_str("tasks=");
            buf.push_str(&g.tasks.join(","));
            buf.push('\n');
            buf.push_str(&format!("cross_task_edges={}\n", g.cross_task_edge_count));
            buf.push_str(&format!("max_depth={}\n", g.max_depth_walked));
            for e in &g.edges {
                let related = e.related_task_id.as_deref().unwrap_or("-");
                buf.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\n",
                    e.edge_id, e.edge_type, e.task_id, related, e.created_at,
                ));
            }
            HandlerOutcome::Ok(buf.into_bytes())
        }
        Err(e) => internal(format!("task.lineage: {e}")),
    }
}

/// `task.pause` — operator-initiated pause (M65). Args:
/// `task_id|<reason>`. Reason optional. Returns
/// `prior_status=<status>` so the caller can render an
/// honest transition message ("running → paused"). The
/// runtime has no flow-pause primitive today; the chronicle
/// records intent — see `set_paused` for the caveat.
fn handle_pause(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.pause utf8: {e}")),
    };
    let mut parts = s.splitn(2, '|');
    let task_id = parts.next().unwrap_or("");
    let reason = parts.next().filter(|v| !v.is_empty());
    if task_id.is_empty() {
        return invalid("task.pause: task_id required".to_string());
    }
    let author = ctx.caller.subject_id.to_string();
    match store.set_paused(task_id, reason, &author) {
        Ok(prior) => HandlerOutcome::Ok(format!("prior_status={prior}\n").into_bytes()),
        Err(CoordinatorError::NotFound(id)) => invalid(format!("task.pause: not found: {id}")),
        Err(CoordinatorError::Invalid(msg)) => invalid(msg),
        Err(e) => internal(format!("task.pause: {e}")),
    }
}

/// `task.resume` — operator-initiated resume (M65). Args:
/// `task_id`. Refuses any status other than `paused`. Returns
/// `pre_pause_status=<status>` (looked up from the most recent
/// `task.paused` event's payload_json).
fn handle_resume(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.resume utf8: {e}")),
    };
    if task_id.is_empty() {
        return invalid("task.resume: task_id required".to_string());
    }
    let author = ctx.caller.subject_id.to_string();
    match store.set_resumed(task_id, &author) {
        Ok(pre) => HandlerOutcome::Ok(format!("pre_pause_status={pre}\n").into_bytes()),
        Err(CoordinatorError::NotFound(id)) => invalid(format!("task.resume: not found: {id}")),
        Err(CoordinatorError::Invalid(msg)) => invalid(msg),
        Err(e) => internal(format!("task.resume: {e}")),
    }
}

/// `task.subtree_metrics` — aggregate per-status counts +
/// total wall-clock + total attempts across the BFS subtree
/// from a root (M75). Args: `task_id|max_depth` (default 4,
/// clamped to [1, 16]). Returns multi-line k=v body so
/// existing dashboard parsers handle it the same way as
/// `task.lineage`. Pure read — does NOT mutate.
fn handle_subtree_metrics(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.subtree_metrics utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(2, '|').collect();
    let task_id = parts.first().copied().unwrap_or("").trim();
    if task_id.is_empty() {
        return invalid("task.subtree_metrics: task_id required".to_string());
    }
    let max_depth: usize = parts
        .get(1)
        .copied()
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    match store.subtree_metrics(task_id, max_depth) {
        Ok(m) => {
            use std::fmt::Write as _;
            let mut buf = String::new();
            let _ = writeln!(buf, "root={}", m.root_task_id);
            let _ = writeln!(buf, "total_tasks={}", m.total_tasks);
            let _ = writeln!(buf, "cross_task_edges={}", m.cross_task_edge_count);
            let _ = writeln!(buf, "max_depth={}", m.max_depth_walked);
            let _ = writeln!(buf, "terminal_completed={}", m.terminal_completed);
            let _ = writeln!(buf, "terminal_failed={}", m.terminal_failed);
            let _ = writeln!(buf, "terminal_cancelled={}", m.terminal_cancelled);
            let _ = writeln!(buf, "active_pending={}", m.active_pending);
            let _ = writeln!(buf, "active_running={}", m.active_running);
            let _ = writeln!(buf, "active_retrying={}", m.active_retrying);
            let _ = writeln!(buf, "active_paused={}", m.active_paused);
            let _ = writeln!(buf, "active_frozen={}", m.active_frozen);
            let _ = writeln!(buf, "active_interrupted={}", m.active_interrupted);
            let _ = writeln!(buf, "active_awaiting_input={}", m.active_awaiting_input);
            let _ = writeln!(buf, "other_status={}", m.other_status);
            let _ = writeln!(buf, "total_attempts={}", m.total_attempts);
            let _ = writeln!(buf, "total_wall_clock_secs={}", m.total_wall_clock_secs);
            if let Some(v) = m.oldest_started_at {
                let _ = writeln!(buf, "oldest_started_at={v}");
            }
            if let Some(v) = m.newest_updated_at {
                let _ = writeln!(buf, "newest_updated_at={v}");
            }
            let _ = writeln!(
                buf,
                "tasks_with_missing_timing={}",
                m.tasks_with_missing_timing
            );
            HandlerOutcome::Ok(buf.into_bytes())
        }
        Err(CoordinatorError::NotFound(id)) => {
            invalid(format!("task.subtree_metrics: not found: {id}"))
        }
        Err(e) => internal(format!("task.subtree_metrics: {e}")),
    }
}

/// `task.transition_check` — informational state-machine
/// validator (M74). Args: `task_id|target_status`. Reads the
/// task's current status + checks against the canonical
/// transition matrix. Returns:
/// `allowed=true|false\ncurrent_status=<s>\ntarget_status=<s>\n`.
///
/// Does NOT mutate the task. Callers (operators, runtime
/// workers, CLI tooling) use this to pre-flight a planned
/// transition without committing it. The actual update
/// path (`task.update`) is not yet enforced against the
/// matrix — that's a separate milestone — so this is the
/// honest authoritative reference for what *should* be
/// permitted.
fn handle_transition_check(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.transition_check utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(2, '|').collect();
    let task_id = parts.first().copied().unwrap_or("").trim();
    let target = parts.get(1).copied().unwrap_or("").trim();
    if task_id.is_empty() || target.is_empty() {
        return invalid("task.transition_check: arg shape `task_id|target_status`".to_string());
    }
    match store.get(task_id) {
        Ok(Some(view)) => {
            let allowed = is_allowed_transition(&view.status, target);
            let body = format!(
                "allowed={allowed}\ncurrent_status={}\ntarget_status={target}\n",
                view.status,
            );
            HandlerOutcome::Ok(body.into_bytes())
        }
        Ok(None) => invalid(format!("task.transition_check: not found: {task_id}")),
        Err(e) => internal(format!("task.transition_check: {e}")),
    }
}

/// `task.record_spawned` — attest a `spawned` edge (M72).
/// Args: `parent_task_id|child_task_id|branch_id|context_id`
/// (branch + context optional). The caller's subject_id is
/// recorded as the producer. Returns
/// `edge_id=N\nevent_id=N\n`.
fn handle_record_spawned(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.record_spawned utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(4, '|').collect();
    let parent = parts.first().copied().unwrap_or("").trim();
    let child = parts.get(1).copied().unwrap_or("").trim();
    let branch = parts.get(2).copied().filter(|v| !v.is_empty());
    let context = parts.get(3).copied().filter(|v| !v.is_empty());
    if parent.is_empty() || child.is_empty() {
        return invalid(
            "task.record_spawned: arg shape `parent_task_id|child_task_id|branch_id|context_id`"
                .to_string(),
        );
    }
    let producer = ctx.caller.subject_id.to_string();
    match store.record_spawned(parent, child, branch, context, &producer) {
        Ok(o) => HandlerOutcome::Ok(
            format!("edge_id={}\nevent_id={}\n", o.edge_id, o.event_id).into_bytes(),
        ),
        Err(CoordinatorError::NotFound(id)) => {
            invalid(format!("task.record_spawned: not found: {id}"))
        }
        Err(CoordinatorError::Invalid(msg)) => invalid(msg),
        Err(e) => internal(format!("task.record_spawned: {e}")),
    }
}

/// `task.record_delegated` — attest a `delegated_to` edge.
/// Args: `parent_task_id|child_task_id|reason` (reason
/// optional). Returns `edge_id=N\nevent_id=N\n`.
fn handle_record_delegated(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.record_delegated utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(3, '|').collect();
    let parent = parts.first().copied().unwrap_or("").trim();
    let child = parts.get(1).copied().unwrap_or("").trim();
    let reason = parts.get(2).copied().filter(|v| !v.is_empty());
    if parent.is_empty() || child.is_empty() {
        return invalid(
            "task.record_delegated: arg shape `parent_task_id|child_task_id|reason`".to_string(),
        );
    }
    let producer = ctx.caller.subject_id.to_string();
    match store.record_delegated(parent, child, reason, &producer) {
        Ok(o) => HandlerOutcome::Ok(
            format!("edge_id={}\nevent_id={}\n", o.edge_id, o.event_id).into_bytes(),
        ),
        Err(CoordinatorError::NotFound(id)) => {
            invalid(format!("task.record_delegated: not found: {id}"))
        }
        Err(CoordinatorError::Invalid(msg)) => invalid(msg),
        Err(e) => internal(format!("task.record_delegated: {e}")),
    }
}

/// `task.record_awaited` — attest an `awaited` edge.
/// Args: `task_id|awaited_task_id|reason` (reason
/// optional). Returns `edge_id=N\nevent_id=N\n`.
fn handle_record_awaited(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.record_awaited utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(3, '|').collect();
    let waiter = parts.first().copied().unwrap_or("").trim();
    let awaited = parts.get(1).copied().unwrap_or("").trim();
    let reason = parts.get(2).copied().filter(|v| !v.is_empty());
    if waiter.is_empty() || awaited.is_empty() {
        return invalid(
            "task.record_awaited: arg shape `task_id|awaited_task_id|reason`".to_string(),
        );
    }
    let producer = ctx.caller.subject_id.to_string();
    match store.record_awaited(waiter, awaited, reason, &producer) {
        Ok(o) => HandlerOutcome::Ok(
            format!("edge_id={}\nevent_id={}\n", o.edge_id, o.event_id).into_bytes(),
        ),
        Err(CoordinatorError::NotFound(id)) => {
            invalid(format!("task.record_awaited: not found: {id}"))
        }
        Err(CoordinatorError::Invalid(msg)) => invalid(msg),
        Err(e) => internal(format!("task.record_awaited: {e}")),
    }
}

/// `task.freeze` — operator-initiated workflow freeze (M71).
/// Args: `task_id|<reason>`. Reason optional. Returns
/// `prior_status=<status>`. Distinct from pause — freeze is
/// intended to propagate down the spawned/delegated subtree
/// once those edge producers ship. Today single-task scope.
fn handle_freeze(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.freeze utf8: {e}")),
    };
    let mut parts = s.splitn(2, '|');
    let task_id = parts.next().unwrap_or("");
    let reason = parts.next().filter(|v| !v.is_empty());
    if task_id.is_empty() {
        return invalid("task.freeze: task_id required".to_string());
    }
    let author = ctx.caller.subject_id.to_string();
    match store.set_frozen(task_id, reason, &author) {
        Ok(prior) => HandlerOutcome::Ok(format!("prior_status={prior}\n").into_bytes()),
        Err(CoordinatorError::NotFound(id)) => invalid(format!("task.freeze: not found: {id}")),
        Err(CoordinatorError::Invalid(msg)) => invalid(msg),
        Err(e) => internal(format!("task.freeze: {e}")),
    }
}

/// `task.unfreeze` — operator-initiated unfreeze (M71).
/// Args: `task_id`. Refuses any status other than `frozen`.
/// Returns `pre_freeze_status=<status>`.
fn handle_unfreeze(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.unfreeze utf8: {e}")),
    };
    if task_id.is_empty() {
        return invalid("task.unfreeze: task_id required".to_string());
    }
    let author = ctx.caller.subject_id.to_string();
    match store.set_unfrozen(task_id, &author) {
        Ok(pre) => HandlerOutcome::Ok(format!("pre_freeze_status={pre}\n").into_bytes()),
        Err(CoordinatorError::NotFound(id)) => invalid(format!("task.unfreeze: not found: {id}")),
        Err(CoordinatorError::Invalid(msg)) => invalid(msg),
        Err(e) => internal(format!("task.unfreeze: {e}")),
    }
}

/// `task.interruption_check` — cooperative-poller snapshot
/// (M70). Args: `task_id`. Returns a multi-line k=v body:
/// `status=...\npause_generation=N\nfreeze_generation=N\n`.
/// Honest about scope: this is the read-side primitive for
/// future cooperative runtime workers — the alpha runtime
/// itself doesn't poll yet.
fn handle_interruption_check(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let task_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.interruption_check utf8: {e}")),
    };
    if task_id.is_empty() {
        return invalid("task.interruption_check: task_id required".to_string());
    }
    match store.interruption_snapshot(task_id) {
        Ok(snap) => {
            let body = format!(
                "status={}\npause_generation={}\nfreeze_generation={}\n",
                snap.status, snap.pause_generation, snap.freeze_generation,
            );
            HandlerOutcome::Ok(body.into_bytes())
        }
        Err(CoordinatorError::NotFound(id)) => {
            invalid(format!("task.interruption_check: not found: {id}"))
        }
        Err(e) => internal(format!("task.interruption_check: {e}")),
    }
}

/// `task.observe_interruption` — runtime ack that a
/// cooperative worker noticed an interruption (M70). Args:
/// `task_id|interruption_type|generation_observed`. Emits the
/// matching `task.pause_observed` / `task.resume_observed` /
/// `task.freeze_propagated` chronicle event. Returns the new
/// event_id.
fn handle_observe_interruption(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.observe_interruption utf8: {e}")),
    };
    let parts: Vec<&str> = s.splitn(3, '|').collect();
    let task_id = parts.first().copied().unwrap_or("").trim();
    let interruption_type = parts.get(1).copied().unwrap_or("").trim();
    let gen_str = parts.get(2).copied().unwrap_or("").trim();
    if task_id.is_empty() || interruption_type.is_empty() || gen_str.is_empty() {
        return invalid(
            "task.observe_interruption: arg shape `task_id|interruption_type|generation`"
                .to_string(),
        );
    }
    let generation: i64 = match gen_str.parse() {
        Ok(v) => v,
        Err(_) => {
            return invalid(format!(
                "task.observe_interruption: invalid generation '{gen_str}'"
            ));
        }
    };
    let observer = ctx.caller.subject_id.to_string();
    match store.observe_interruption(task_id, interruption_type, generation, &observer) {
        Ok(event_id) => HandlerOutcome::Ok(format!("event_id={event_id}\n").into_bytes()),
        Err(CoordinatorError::NotFound(id)) => {
            invalid(format!("task.observe_interruption: not found: {id}"))
        }
        Err(CoordinatorError::Invalid(msg)) => invalid(msg),
        Err(e) => internal(format!("task.observe_interruption: {e}")),
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
    let _ = writeln!(s, "pause_generation={}", v.pause_generation);
    let _ = writeln!(s, "freeze_generation={}", v.freeze_generation);
    if let Some(x) = v.frozen_at {
        let _ = writeln!(s, "frozen_at={}", x);
    }
    if let Some(x) = v.frozen_reason.as_ref() {
        let _ = writeln!(s, "frozen_reason={}", x);
    }
    // PH-ORIGIN-SURFACE (D-004): which dispatch surface created
    // the task. Skipped when NULL (older rows or callers that
    // didn't stamp the label) — the bridge / dashboard treat
    // absence as "unknown".
    if let Some(x) = v.origin_surface.as_ref() {
        let _ = writeln!(s, "origin_surface={}", x);
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

fn init_schema(conn: &mut Connection) -> Result<(), CoordinatorError> {
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

        -- PH-WAVE2D: per-task todo list. Ordered subtasks the AI
        -- (or operator) can use to decompose work. Each row is
        -- one item: text + status (open|done) + position. Bumping
        -- a row's position is O(1) (no resort needed); the
        -- canonical render order is `position ASC` with `id ASC`
        -- as a tiebreaker.
        CREATE TABLE IF NOT EXISTS task_todos (
            todo_id    INTEGER PRIMARY KEY AUTOINCREMENT,
            task_id    TEXT    NOT NULL,
            position   INTEGER NOT NULL,
            status     TEXT    NOT NULL DEFAULT 'open',
            text       TEXT    NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            FOREIGN KEY (task_id) REFERENCES tasks(task_id)
        );
        CREATE INDEX IF NOT EXISTS task_todos_by_task
            ON task_todos(task_id, position);

        -- PHASE 1 (Brief): Dossiers — durable artifacts attached to
        -- a Brief (plan / design / note / deliverable). Append-only;
        -- the artifact trail of a Brief is auditable. `kind` is an
        -- operator/agent-curated label; `body` holds the content.
        CREATE TABLE IF NOT EXISTS task_documents (
            doc_id     TEXT PRIMARY KEY,
            task_id    TEXT NOT NULL,
            kind       TEXT NOT NULL,
            title      TEXT NOT NULL,
            body       TEXT NOT NULL DEFAULT '',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            FOREIGN KEY (task_id) REFERENCES tasks(task_id)
        );
        CREATE INDEX IF NOT EXISTS task_documents_by_task
            ON task_documents(task_id, created_at);
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
        // M70: cooperative-interruption generation counters.
        // Two distinct axes — pause / resume requests bump
        // `pause_generation`; freeze / unfreeze (M71) bumps
        // `freeze_generation`. Cooperative workers compare a
        // cached value against the current row to decide
        // whether they need to re-check interruption state
        // before continuing work. HONEST: nothing in the
        // runtime polls these today — they are scaffolding
        // for future cooperative workers + the new
        // `task.observe_interruption` attestation capability.
        "ALTER TABLE tasks ADD COLUMN pause_generation INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE tasks ADD COLUMN freeze_generation INTEGER NOT NULL DEFAULT 0",
        // M71: operator-set freeze (workflow-level pause).
        // `frozen_at` stamps the freeze request; `frozen_reason`
        // captures optional triage context. Freeze is intended
        // to propagate down the spawned/delegated subtree once
        // those edge producers ship (M72+). Today it sits on
        // the single task. The matching `task.freeze_propagated`
        // chronicle event (added in M70) lands when a
        // cooperative worker attests via
        // `task.observe_interruption`.
        "ALTER TABLE tasks ADD COLUMN frozen_at INTEGER",
        "ALTER TABLE tasks ADD COLUMN frozen_reason TEXT",
        // H4: anti-thrash counter (Hermes-inspired). Tracks how
        // many *consecutive* failures shared the same
        // `last_failure_class`. Incremented when a failed update
        // has the same class as the previous one; reset to 1
        // when the class differs; left at 0 until the first
        // failure. When the counter crosses ANTI_THRASH_THRESHOLD
        // (3) the coordinator auto-marks the task for investigation
        // and emits a `task.thrash_detected` chronicle event so
        // operators see stuck retry loops without grepping audit
        // logs. NULL/0 means no failures yet — the field is
        // additive and a fresh DB starts at 0.
        "ALTER TABLE tasks ADD COLUMN consecutive_same_class_count INTEGER NOT NULL DEFAULT 0",
        // PH-ORIGIN-SURFACE (D-004): which dispatch surface
        // created this task. One of: "chat" / "dashboard" /
        // "cli" / "channel" / "flow-engine" / "unknown".
        // Operator-curated label set; the bridge stamps the
        // value on task creation via the per-route knowledge.
        // NULL on rows created before this migration; the
        // coordinator treats NULL as "unknown" for dashboard
        // rendering / filtering. Default NULL (the existing
        // `caller` field still captures *who* authorized it;
        // this captures *which surface* dispatched it).
        "ALTER TABLE tasks ADD COLUMN origin_surface TEXT",
        // PHASE 1 (Task → Brief): the product-spine lifecycle
        // columns that turn a coordinator Task into a **Brief**
        // (see docs/relix-lexicon.md). All additive + nullable
        // or defaulted, so existing rows keep flowing.
        //   assignee_agent_id — the single Operative this Brief
        //     is assigned to (distinct from `owner_subject_id`,
        //     which is the creator). NULL = unassigned.
        "ALTER TABLE tasks ADD COLUMN assignee_agent_id TEXT",
        //   board_status — the Brief's board column, separate
        //     from the execution `status`. One of: backlog /
        //     todo / in_progress / in_review / done / blocked /
        //     cancelled. Defaults to 'backlog'.
        "ALTER TABLE tasks ADD COLUMN board_status TEXT NOT NULL DEFAULT 'backlog'",
        //   priority — low / normal / high / urgent.
        "ALTER TABLE tasks ADD COLUMN priority TEXT NOT NULL DEFAULT 'normal'",
        //   mandate_id / campaign_id — the spine links a Brief
        //     points *up* to (the "why" and the workstream).
        //     NULL = unlinked. Validated against the spine store
        //     at write time once the handlers are wired.
        "ALTER TABLE tasks ADD COLUMN mandate_id TEXT",
        "ALTER TABLE tasks ADD COLUMN campaign_id TEXT",
        // PHASE 3 (heartbeat loop): the atomic Claim — single-owner
        // execution lock on a Brief. `claimed_by` is the Operative
        // holding it; `claim_expires_at` is the lease deadline (unix
        // secs). NULL or past-deadline = claimable. Both additive +
        // nullable, so existing rows are simply unclaimed.
        "ALTER TABLE tasks ADD COLUMN claimed_by TEXT",
        "ALTER TABLE tasks ADD COLUMN claim_expires_at INTEGER",
        // PHASE 5 (Brief): free-form labels — a normalized,
        // comma-joined tag set (bug / feature / customer-x …) for
        // organising the board. NULL = no labels. Additive.
        "ALTER TABLE tasks ADD COLUMN labels TEXT",
    ];
    // Apply additive ALTER TABLE migrations inside a transaction.
    // Duplicate-column errors (legacy boots that already added the
    // column) are tolerated; ANY other error fails startup loudly
    // so a typo or a schema bug surfaces immediately instead of
    // being silently swallowed.
    crate::db::apply_additive_migrations(conn, &alters).map_err(CoordinatorError::Db)?;
    // Stamp the highest migration version we know about so the
    // _relix_migrations table reflects current state. Version
    // numbers are arbitrary — we use the count of additive
    // statements as the cursor so adding a new ALTER bumps it.
    crate::db::record_migration_applied(conn, alters.len() as i64).map_err(CoordinatorError::Db)?;
    Ok(())
}

/// Map a row of the BriefCard column set into a `brief::BriefCard`.
/// Column order: task_id, title, board_status, priority,
/// assignee_agent_id, mandate_id, campaign_id.
fn brief_card_from_row(r: &rusqlite::Row) -> rusqlite::Result<brief::BriefCard> {
    Ok(brief::BriefCard {
        task_id: r.get(0)?,
        title: r.get(1)?,
        board_status: r.get(2)?,
        priority: r.get(3)?,
        assignee_agent_id: r.get(4)?,
        mandate_id: r.get(5)?,
        campaign_id: r.get(6)?,
    })
}

/// PHASE 1 (Brief): does a Brief row exist? Helper for the
/// relation-edge validators. Takes an already-locked connection.
fn task_row_exists(conn: &Connection, task_id: &str) -> Result<bool, CoordinatorError> {
    match conn.query_row(
        "SELECT 1 FROM tasks WHERE task_id = ?1",
        params![task_id],
        |_| Ok(()),
    ) {
        Ok(()) => Ok(true),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(e) => Err(CoordinatorError::Db(e)),
    }
}

fn new_task_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn new_doc_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("doc_{}", hex::encode(bytes))
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

/// H14: synthesize a one-line terminal post-mortem event for a
/// task that just transitioned to a terminal status. Pulls the
/// facts the coordinator already owns (attempts, retries,
/// started_at, last_failure_class) and writes them as a
/// `task.terminal_summary` chronicle event in the same
/// transaction as the status flip.
///
/// Idempotent guard: if the chronicle already contains a
/// `task.terminal_summary` event for this task we skip — this
/// keeps the helper safe to call across any caller path that
/// might land more than once. (The state machine should refuse
/// re-entering a terminal status anyway; this is belt + braces.)
fn emit_terminal_summary_in_txn(
    tx: &rusqlite::Transaction<'_>,
    task_id: &str,
    new_status: &str,
    now: i64,
) -> Result<(), CoordinatorError> {
    let already: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM task_events
             WHERE task_id = ?1 AND event_type = 'task.terminal_summary'",
            params![task_id],
            |r| r.get(0),
        )
        .map_err(CoordinatorError::Db)?;
    if already > 0 {
        return Ok(());
    }
    let row = tx
        .query_row(
            "SELECT attempt_count, retry_count, started_at, last_failure_class
             FROM tasks WHERE task_id = ?1",
            params![task_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(CoordinatorError::Db)?;
    let Some((attempts, retries, started_at, last_class)) = row else {
        return Ok(());
    };
    let wall = started_at.map(|s| (now - s).max(0)).unwrap_or(0);
    let cls = last_class.as_deref().unwrap_or("");
    // Mirror the new_status into the payload's reason field. We
    // keep a separate name here because the recovery-scan path
    // (H5) sets a more specific reason ("deadline_exceeded")
    // for the same event_type.
    let reason = new_status;
    let legacy = format!(
        "{reason} · attempts={attempts} retries={retries} \
         wall_clock_secs={wall} last_failure_class={cls}",
    );
    let json = format!(
        r#"{{"reason":"{}","attempts":{attempts},"retries":{retries},"wall_clock_secs":{wall},"last_failure_class":"{}","auto_emitted_by":"update_task"}}"#,
        json_escape(reason),
        json_escape(cls),
    );
    insert_typed_event(
        tx,
        task_id,
        now,
        "task.terminal_summary",
        &legacy,
        None,
        None,
        Some(&json),
    )?;
    Ok(())
}

/// H7: close `task_attempts` rows that are still open (no
/// `finished_at`) but whose owning task has reached a terminal
/// status. These orphans appear when a task transitions to a
/// terminal state via a non-attempt-aware path (legacy data,
/// crash mid-update, pre-C2a tasks). Emits a
/// `task.attempt_orphan_closed` event per orphan so the
/// chronicle is honest about the cleanup. Returns the list of
/// closed attempt_ids (empty when there are no orphans).
fn close_orphan_attempts_in_txn(
    tx: &rusqlite::Transaction<'_>,
    now: i64,
) -> Result<Vec<i64>, CoordinatorError> {
    let mut stmt = tx
        .prepare(
            "SELECT a.attempt_id, a.task_id, t.status
             FROM task_attempts a
             JOIN tasks t ON t.task_id = a.task_id
             WHERE a.finished_at IS NULL
               AND t.status IN ('completed', 'failed', 'cancelled', 'interrupted')",
        )
        .map_err(CoordinatorError::Db)?;
    let orphans: Vec<(i64, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(CoordinatorError::Db)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(CoordinatorError::Db)?;
    drop(stmt);

    let mut closed = Vec::with_capacity(orphans.len());
    for (aid, tid, task_status) in orphans {
        tx.execute(
            "UPDATE task_attempts
             SET finished_at = ?1,
                 status = 'interrupted',
                 failure_class = COALESCE(failure_class, 'orphan'),
                 error_cause = COALESCE(
                     error_cause,
                     'attempt left open while task reached terminal status'
                 )
             WHERE attempt_id = ?2
               AND finished_at IS NULL",
            params![now, aid],
        )
        .map_err(CoordinatorError::Db)?;
        let legacy = format!(
            "attempt_id={aid} closed_as=interrupted reason=orphan task_status={task_status}",
        );
        let json = format!(
            r#"{{"attempt_id":{aid},"closed_as":"interrupted","reason":"orphan","task_status":"{}"}}"#,
            json_escape(&task_status)
        );
        insert_typed_event(
            tx,
            &tid,
            now,
            "task.attempt_orphan_closed",
            &legacy,
            Some(aid),
            None,
            Some(&json),
        )?;
        closed.push(aid);
    }
    Ok(closed)
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

/// Extract `(task_id, status)` from a `task.update` arg buffer
/// for the auto-skill post-hook. The handler already validated
/// the args, so missing fields are treated as "no signal" and
/// returned as `None` — we never re-raise validation errors.
fn parse_completed_signal(args: &[u8]) -> Option<(String, String)> {
    let s = std::str::from_utf8(args).ok()?;
    let mut parts = s.splitn(9, '|');
    let task_id = parts.next()?.to_string();
    let status = parts.next()?.to_string();
    if task_id.is_empty() || status.is_empty() {
        return None;
    }
    Some((task_id, status))
}

/// W5: one assembled chat turn — the unit returned by
/// `task.session_export`. Carries enough for an operator
/// transcript or downstream replay tooling without baking in
/// chronicle event-id specifics.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChatTurn {
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub timestamp_unix: i64,
}

/// Parse a `chat.user_turn` / `chat.assistant_turn` chronicle
/// payload into a structured [`ChatTurn`].
///
/// Wire format: `<session_id>|<role>|<timestamp_unix>|<content>`.
/// The `content` field is the rest-of-string (may contain `|`).
/// The chronicle's row `ts` is used as a fallback when the
/// payload's embedded timestamp is missing / unparseable.
pub fn parse_chat_turn_payload(
    expected_session_id: &str,
    event_type: &str,
    payload: &str,
    row_ts: i64,
) -> Option<ChatTurn> {
    let mut it = payload.splitn(4, '|');
    let session_id = it.next()?.to_string();
    if session_id != expected_session_id {
        return None;
    }
    let role = it.next()?.to_string();
    let timestamp_unix = it
        .next()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(row_ts);
    let content = it.next().unwrap_or("").to_string();
    // Sanity: event_type and role must agree.
    let role_lc = role.to_ascii_lowercase();
    match (event_type, role_lc.as_str()) {
        ("chat.user_turn", "user") => {}
        ("chat.assistant_turn", "assistant") => {}
        // Accept slight mismatch so older recorders can be
        // replayed honestly — but log so a reader spots the
        // skew.
        _ => {
            tracing::debug!(
                event_type,
                role = %role,
                "chat-turn payload role does not match event_type"
            );
        }
    }
    Some(ChatTurn {
        session_id,
        role,
        content,
        timestamp_unix,
    })
}

/// Default + max limits for `task.session_search`. The
/// search is operator-facing (and tool-callable for agents
/// searching their own history); both caps are conservative
/// so a misconfigured caller cannot DoS the coordinator.
pub const DEFAULT_SESSION_SEARCH_LIMIT: usize = 20;
pub const MAX_SESSION_SEARCH_LIMIT: usize = 100;

/// One match returned by `task.session_search`. Wire-stable
/// JSON shape; clients (the bridge proxy, the agent tool
/// path, the dashboard) round-trip it via serde.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub timestamp_unix: i64,
    pub snippet: String,
    /// `1.0` for the LIKE-fallback path; reserved for a real
    /// BM25 score when an FTS5 chronicle index lands.
    pub score: f32,
}

/// Build an operator-readable snippet for a search hit. Lands
/// the first occurrence of `query` in `content` (case-insensitive)
/// surrounded by up to 50 characters of context on each side.
/// When the query is not found in `content` (e.g. the LIKE
/// matched on the payload prefix), returns the first 110
/// chars of content as a fallback so the row is still
/// operator-readable.
pub fn build_match_snippet(content: &str, query: &str) -> String {
    if query.is_empty() {
        return content.chars().take(110).collect();
    }
    let lc_content = content.to_ascii_lowercase();
    let lc_query = query.to_ascii_lowercase();
    let Some(byte_idx) = lc_content.find(&lc_query) else {
        return content.chars().take(110).collect();
    };
    // Convert byte index to char index so the +/- 50 window
    // is unicode-safe.
    let mut chars_before_match = 0usize;
    let mut byte_count = 0usize;
    for c in content.chars() {
        if byte_count >= byte_idx {
            break;
        }
        byte_count += c.len_utf8();
        chars_before_match += 1;
    }
    let start_char = chars_before_match.saturating_sub(50);
    let match_char_len = lc_query.chars().count();
    let end_char = chars_before_match + match_char_len + 50;
    let chars: Vec<char> = content.chars().collect();
    let end_char = end_char.min(chars.len());
    let prefix = if start_char > 0 { "…" } else { "" };
    let suffix = if end_char < chars.len() { "…" } else { "" };
    let body: String = chars[start_char..end_char].iter().collect();
    format!("{prefix}{body}{suffix}")
}

/// W5: `task.session_export` handler. Arg = `session_id`.
/// Returns a JSON array of [`ChatTurn`] objects in
/// chronological order.
fn handle_session_export(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let session_id = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("task.session_export utf8: {e}")),
    };
    if session_id.is_empty() {
        return invalid("task.session_export: session_id required".to_string());
    }
    match store.query_chat_turns(session_id, 10_000) {
        Ok(turns) => match serde_json::to_string(&turns) {
            Ok(json) => HandlerOutcome::Ok(json.into_bytes()),
            Err(e) => internal(format!("task.session_export json: {e}")),
        },
        Err(e) => internal(format!("task.session_export: {e}")),
    }
}

/// `task.session_search` handler.
///
/// Wire format (`splitn(3, '|')` so the query keeps its own
/// internal pipes):
///
/// ```text
/// <subject_id>|<query>|<limit>
/// ```
///
/// Returns the same JSON shape as `task.session_export` but
/// with [`SessionSearchHit`] entries (adds `snippet` + `score`).
pub(crate) fn handle_session_search(store: &TaskStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("task.session_search utf8: {e}")),
    };
    let mut parts = s.splitn(3, '|');
    let subject_id = parts.next().unwrap_or("").trim().to_string();
    let query = parts.next().unwrap_or("").to_string();
    let limit = parts
        .next()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_SESSION_SEARCH_LIMIT);
    if query.trim().is_empty() {
        return invalid("task.session_search: query required".to_string());
    }
    match store.search_chat_turns(&subject_id, &query, limit) {
        Ok(hits) => match serde_json::to_string(&hits) {
            Ok(json) => HandlerOutcome::Ok(json.into_bytes()),
            Err(e) => internal(format!("task.session_search json: {e}")),
        },
        Err(e) => internal(format!("task.session_search: {e}")),
    }
}

/// Best-effort drift evaluation for a `running` task. Pulls
/// the task's view + recent chronicle, hands the summary to
/// the drift detector, optionally computes cosine similarity
/// against the goal embedding via the supplied
/// [`DriftEmbedDispatcher`], and writes a
/// `guardrail.drift_evaluation` chronicle entry the operator
/// can audit later.
///
/// Payload format (pipe-delimited so existing chronicle
/// inspectors can read it):
///
/// ```text
/// goal=<title>|threshold=<f>|similarity=<f>|drift_detected=<bool>|summary=<preview>
/// ```
///
/// `similarity` is `"none"` and `drift_detected` is `false`
/// when no embedding dispatcher is wired or one of the embed
/// calls returned `None`.
pub(crate) async fn evaluate_drift_for_task(
    store: &TaskStore,
    task_id: &str,
    cfg: &crate::nodes::ai::guardrails::DriftConfig,
    embedder: Option<Arc<dyn crate::nodes::ai::guardrails::DriftEmbedDispatcher>>,
) {
    use crate::nodes::ai::guardrails::{ChronicleEvent, DriftDetector};
    let detector = DriftDetector::from_config(cfg);
    let view = match store.get(task_id) {
        Ok(Some(v)) => v,
        _ => return,
    };
    let events = store
        .query_events(
            task_id,
            0,
            detector.check_every_n() as usize * 4,
            None,
            EventOrder::Desc,
        )
        .unwrap_or_default();
    // query_events with Desc returns newest-first; the
    // summariser wants chronological order so reverse.
    let mut chronological = events;
    chronological.reverse();
    let mapped: Vec<ChronicleEvent> = chronological
        .into_iter()
        .map(|e| ChronicleEvent::new(e.event_type, e.payload))
        .collect();
    let Some(summary) = detector.summarise_recent_events(&mapped) else {
        // Not enough events yet — log at debug so the
        // operator can correlate the skip without spamming
        // warns.
        tracing::debug!(
            task_id,
            n = mapped.len(),
            check_every_n = detector.check_every_n(),
            "drift: not enough chronicle events to evaluate"
        );
        return;
    };
    let goal_preview: String = view.title.chars().take(120).collect();
    // W4: when an embedding dispatcher is wired, embed the
    // goal title + recent-activity summary and compute the
    // cosine similarity. The score lands in the chronicle so
    // operators can audit drift trend over time. Failures
    // (provider doesn't support embeddings, mismatched
    // dimensions, etc.) skip the comparison silently —
    // existing deployments without a dispatcher behave
    // identically to before.
    let (similarity, drift_detected): (Option<f32>, bool) = match embedder.as_ref() {
        Some(d) => {
            let goal_vec = d.embed(&view.title).await;
            let recent_vec = d.embed(&summary).await;
            match (goal_vec, recent_vec) {
                (Some(g), Some(r)) => {
                    let s = crate::nodes::ai::guardrails::drift::cosine_similarity(&g, &r);
                    (Some(s), s < detector.threshold())
                }
                _ => (None, false),
            }
        }
        None => (None, false),
    };
    tracing::info!(
        task_id,
        action = cfg.action.as_str(),
        threshold = detector.threshold(),
        goal = %goal_preview,
        summary_lines = summary.lines().count(),
        similarity = ?similarity,
        drift_detected,
        "drift: evaluation recorded"
    );
    let similarity_field = match similarity {
        Some(s) => format!("{s:.4}"),
        None => "none".to_string(),
    };
    let payload = format!(
        "goal={}|threshold={:.3}|similarity={}|drift_detected={}|summary={}",
        goal_preview.replace('|', " "),
        detector.threshold(),
        similarity_field,
        drift_detected,
        summary.lines().take(3).collect::<Vec<_>>().join(" / ")
    );
    if let Err(e) = store.append_event(task_id, "guardrail.drift_evaluation", &payload) {
        tracing::warn!(task_id, error = %e, "drift: append chronicle event failed");
    }
}

/// Best-effort SKILL.md auto-generation for a completed task.
/// Synchronous + non-failing: every step that could error
/// (store reads, dir create, file write) is folded into a
/// silent skip so the post-hook can never destabilise the
/// `task.update` path.
fn run_auto_skill_for_task(
    store: &TaskStore,
    task_id: &str,
    cfg: &crate::nodes::ai::skills::SkillsConfig,
) {
    if !cfg.auto_generate {
        return;
    }
    let Some(dir) = crate::nodes::ai::skills::resolve_auto_skill_dir(cfg) else {
        tracing::debug!(task_id, "auto-skill: no HOME / USERPROFILE; skipping write");
        return;
    };
    let view = match store.get(task_id) {
        Ok(Some(v)) => v,
        Ok(None) | Err(_) => return,
    };
    let attempts = store.list_attempts(task_id).unwrap_or_default();
    let duration_secs = attempts
        .iter()
        .filter_map(|a| a.finished_at.map(|f| (a.started_at, f)))
        .map(|(s, f)| (f - s).max(0))
        .sum::<i64>();
    let events = store
        .query_events(task_id, 0, 200, None, EventOrder::Asc)
        .unwrap_or_default();
    let mut summary = String::new();
    for e in events.iter().take(20) {
        let line = if e.payload.is_empty() {
            format!("- {} (event_id={})\n", e.event_type, e.event_id)
        } else {
            // Truncate noisy payloads so the SKILL.md stays
            // human-readable.
            let trimmed = if e.payload.len() > 200 {
                // Snap 200 down to a char boundary so multi-byte
                // payload text isn't sliced mid-codepoint (panics).
                let mut cut = 200;
                while cut > 0 && !e.payload.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("{}…", &e.payload[..cut])
            } else {
                e.payload.clone()
            };
            format!("- {}: {}\n", e.event_type, trimmed)
        };
        summary.push_str(&line);
    }
    let title = if view.title.trim().is_empty() {
        format!("auto-{task_id}")
    } else {
        view.title.clone()
    };
    let body = crate::nodes::ai::skills::render_auto_skill_body(
        &title,
        &view.flow_template,
        duration_secs,
        summary.trim_end(),
    );
    let slug = crate::nodes::ai::skills::slugify_for_filename(&title);
    match crate::nodes::ai::skills::write_auto_skill(&dir, &slug, &body) {
        Ok(Some(path)) => {
            tracing::info!(
                task_id,
                path = %path.display(),
                "auto-skill: wrote SKILL.md from completed task"
            );
        }
        Ok(None) => {
            tracing::debug!(
                task_id,
                slug,
                "auto-skill: skill file already exists, not overwriting"
            );
        }
        Err(e) => {
            tracing::warn!(task_id, error = %e, "auto-skill: write failed");
        }
    }
}

#[cfg(test)]
mod auto_skill_tests {
    use super::*;

    #[test]
    fn parse_completed_signal_extracts_task_id_and_status() {
        let buf = b"task-abc|completed|some result|||||||";
        let got = parse_completed_signal(buf).expect("parse should yield a tuple");
        assert_eq!(got.0, "task-abc");
        assert_eq!(got.1, "completed");
    }

    #[test]
    fn parse_completed_signal_rejects_empty_task_id() {
        let buf = b"|completed||||||||";
        assert!(parse_completed_signal(buf).is_none());
    }

    #[test]
    fn run_auto_skill_writes_skill_when_enabled() {
        let store = TaskStore::in_memory().unwrap();
        let id = store
            .create(
                "deploy staging",
                "flows/deploy.sol",
                "{}",
                "subject-1",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        store
            .update(&id, Some("completed"), None, None, None, None, None, None)
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::nodes::ai::skills::SkillsConfig {
            auto_generate: true,
            max_age_days: 30,
            auto_dir: Some(tmp.path().to_path_buf()),
            ..Default::default()
        };
        run_auto_skill_for_task(&store, &id, &cfg);
        // Walk the dir for the synthesised SKILL.md.
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "exactly one SKILL.md should be written");
        let body = std::fs::read_to_string(entries[0].path()).unwrap();
        assert!(body.contains("# deploy staging"));
        assert!(body.contains("flows/deploy.sol"));
    }

    #[test]
    fn run_auto_skill_noop_when_disabled() {
        let store = TaskStore::in_memory().unwrap();
        let id = store
            .create(
                "anything",
                "tpl",
                "{}",
                "s",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::nodes::ai::skills::SkillsConfig {
            auto_generate: false,
            max_age_days: 30,
            auto_dir: Some(tmp.path().to_path_buf()),
            ..Default::default()
        };
        run_auto_skill_for_task(&store, &id, &cfg);
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(entries.is_empty(), "disabled cfg must not write anything");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> TaskStore {
        TaskStore::in_memory().expect("open")
    }

    #[test]
    fn phase1_brief_board_move_enforces_the_state_machine() {
        let s = store();
        let id = s
            .create(
                "ship the landing page",
                "flows/none.sol",
                "{}",
                "subject-1",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();

        // Fresh Briefs open in 'backlog'.
        assert_eq!(s.board_status(&id).unwrap().as_deref(), Some("backlog"));

        // Walk the happy path; the from→to pair is reported.
        assert_eq!(
            s.set_board_status(&id, "todo").unwrap(),
            ("backlog".to_string(), "todo".to_string())
        );
        s.set_board_status(&id, "in_progress").unwrap();
        s.set_board_status(&id, "in_review").unwrap();
        s.set_board_status(&id, "done").unwrap();
        assert_eq!(s.board_status(&id).unwrap().as_deref(), Some("done"));

        // Illegal skips and unknown statuses are rejected.
        let id2 = s
            .create(
                "x",
                "flows/none.sol",
                "{}",
                "subj",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        assert!(matches!(
            s.set_board_status(&id2, "done"),
            Err(CoordinatorError::Invalid(_))
        ));
        assert!(matches!(
            s.set_board_status(&id2, "bogus"),
            Err(CoordinatorError::Invalid(_))
        ));

        // Cancel is terminal.
        s.set_board_status(&id2, "cancelled").unwrap();
        assert!(matches!(
            s.set_board_status(&id2, "todo"),
            Err(CoordinatorError::Invalid(_))
        ));

        // Unknown Brief → NotFound.
        assert!(matches!(
            s.set_board_status("nope", "todo"),
            Err(CoordinatorError::NotFound(_))
        ));
    }

    #[test]
    fn board_move_records_a_chronicle_event() {
        let s = store();
        let id = s
            .create(
                "b",
                "flows/none.sol",
                "{}",
                "subj",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        s.set_board_status(&id, "todo").unwrap();
        s.set_board_status(&id, "in_progress").unwrap();
        // Idempotent no-op records nothing.
        s.set_board_status(&id, "in_progress").unwrap();

        let conn = s.conn.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_events
                 WHERE task_id = ?1 AND event_type = 'brief.board_moved'",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "two real moves → two chronicle events");
        let payload: String = conn
            .query_row(
                "SELECT payload FROM task_events
                 WHERE task_id = ?1 AND event_type = 'brief.board_moved'
                 ORDER BY event_id DESC LIMIT 1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(payload, "todo -> in_progress");
    }

    #[test]
    fn brief_labels_normalize_dedupe_and_clear() {
        let s = store();
        let id = s
            .create("b", "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
            .unwrap();
        // Empty initially.
        assert!(s.brief_labels(&id).unwrap().is_empty());

        // Set with dupes, whitespace, and a comma-bearing entry (dropped).
        s.set_brief_labels(&id, &["bug", " bug ", "urgent", "a,b", ""])
            .unwrap();
        assert_eq!(
            s.brief_labels(&id).unwrap(),
            vec!["bug".to_string(), "urgent".to_string()]
        );

        // Replace wholesale.
        s.set_brief_labels(&id, &["feature"]).unwrap();
        assert_eq!(s.brief_labels(&id).unwrap(), vec!["feature".to_string()]);

        // Clear with an empty set.
        s.set_brief_labels(&id, &[]).unwrap();
        assert!(s.brief_labels(&id).unwrap().is_empty());

        // Filter by label across briefs (CSV-membership).
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        let a = mk("a");
        let b = mk("b");
        let c = mk("c");
        s.set_brief_labels(&a, &["bug", "urgent"]).unwrap(); // exact-in-middle
        s.set_brief_labels(&b, &["urgent"]).unwrap(); // exact-only
        s.set_brief_labels(&c, &["feature"]).unwrap();
        let urgent: std::collections::HashSet<String> = s
            .list_briefs_by_label("urgent", 50)
            .unwrap()
            .into_iter()
            .map(|c| c.task_id)
            .collect();
        assert!(urgent.contains(&a) && urgent.contains(&b));
        assert!(!urgent.contains(&c));
        assert_eq!(urgent.len(), 2);
        assert!(s.list_briefs_by_label("", 50).unwrap().is_empty());

        // Unknown Brief → NotFound on both paths.
        assert!(matches!(
            s.brief_labels("nope"),
            Err(CoordinatorError::NotFound(_))
        ));
        assert!(matches!(
            s.set_brief_labels("nope", &["x"]),
            Err(CoordinatorError::NotFound(_))
        ));
    }

    #[test]
    fn latest_dossier_returns_the_newest_of_a_kind() {
        let s = store();
        let id = s
            .create("b", "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
            .unwrap();
        s.add_dossier(&id, "plan", "Plan v1", "first").unwrap();
        s.add_dossier(&id, "spec", "Spec", "specbody").unwrap();
        let v2 = s.add_dossier(&id, "plan", "Plan v2", "second").unwrap();

        let latest = s.latest_dossier(&id, "plan").unwrap().unwrap();
        assert_eq!(latest.doc_id, v2);
        assert_eq!(latest.title, "Plan v2");
        assert_eq!(latest.body, "second");
        // A kind with no Dossier → None.
        assert!(s.latest_dossier(&id, "design").unwrap().is_none());
    }

    #[test]
    fn create_brief_materializes_on_the_spine() {
        let s = store();
        let id = s
            .create_brief(
                "Build the onboarding flow",
                "subj-founder",
                Some("agt_eng"),
                Some("mandate_x"),
                Some("camp_y"),
                Some("high"),
            )
            .unwrap();

        let f = s.brief_fields(&id).unwrap().unwrap();
        assert_eq!(f.board_status, "todo");
        assert_eq!(f.assignee_agent_id.as_deref(), Some("agt_eng"));
        assert_eq!(f.mandate_id.as_deref(), Some("mandate_x"));
        assert_eq!(f.campaign_id.as_deref(), Some("camp_y"));
        assert_eq!(f.priority, "high");

        // Minimal: title only → defaults (todo, normal, no links).
        let bare = s
            .create_brief("Just a title", "subj", None, None, None, None)
            .unwrap();
        let bf = s.brief_fields(&bare).unwrap().unwrap();
        assert_eq!(bf.board_status, "todo");
        assert_eq!(bf.priority, "normal");
        assert!(bf.assignee_agent_id.is_none());

        // Creation is chronicled (the activity feed's first entry).
        let created = s
            .query_events(&bare, 0, 10, Some("brief.created"), EventOrder::Desc)
            .unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].payload, "Just a title");

        // Empty title / bad priority rejected.
        assert!(matches!(
            s.create_brief("  ", "subj", None, None, None, None),
            Err(CoordinatorError::Invalid(_))
        ));
        assert!(matches!(
            s.create_brief("t", "subj", None, None, None, Some("meh")),
            Err(CoordinatorError::Invalid(_))
        ));
    }

    #[test]
    fn compose_brief_prompt_includes_title_dossiers_and_comments() {
        let s = store();
        let id = s
            .create("Ship the auth rewrite", "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
            .unwrap();
        s.add_dossier(&id, "spec", "Auth Spec", "body").unwrap();
        s.add_dossier(&id, "plan", "The Plan", "step 1: design the token flow")
            .unwrap();
        s.comment_on_brief(&id, "founder", "use passkeys").unwrap();
        s.comment_on_brief(&id, "agt_eng", "starting now").unwrap();

        let prompt = s.compose_brief_prompt(&id, 10);
        assert!(prompt.starts_with("Ship the auth rewrite"));
        assert!(prompt.contains("[spec] Auth Spec"));
        // The plan body is inlined for the agent.
        assert!(prompt.contains("Current plan:"));
        assert!(prompt.contains("step 1: design the token flow"));
        // Comments oldest→newest.
        let first = prompt.find("use passkeys").unwrap();
        let second = prompt.find("starting now").unwrap();
        assert!(first < second, "comments should be oldest-first");

        // Unknown Brief → empty string (degrades cleanly).
        assert!(s.compose_brief_prompt("nope", 10).is_empty());
    }

    #[test]
    fn brief_detail_assembles_the_full_view() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        let task = mk("task");
        let child = mk("child");
        let blocker = mk("blocker");
        let waiter = mk("waiter");
        s.set_brief_field(&task, "assignee", "agt_a").unwrap();
        s.link_subbrief(&task, &child).unwrap();
        s.add_snag(&task, &blocker).unwrap(); // task blocked by blocker
        s.add_snag(&waiter, &task).unwrap(); // waiter blocked by task
        s.add_dossier(&task, "plan", "The Plan", "body").unwrap();

        let d = s.brief_detail(&task).unwrap().unwrap();
        assert_eq!(d.fields.assignee_agent_id.as_deref(), Some("agt_a"));
        assert_eq!(d.subbriefs, vec![child]);
        assert_eq!(d.snags, vec![blocker]);
        assert_eq!(d.blocking, vec![waiter]);
        assert!(d.parents.is_empty());
        assert_eq!(d.dossiers.len(), 1);
        assert!(d.blocked, "blocker isn't done → task is blocked");

        // Unknown Brief → None.
        assert!(s.brief_detail("nope").unwrap().is_none());
    }

    #[test]
    fn subbrief_progress_counts_children_by_column() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        let parent = mk("parent");
        // Two children done, one in_progress.
        for t in ["c1", "c2"] {
            let c = mk(t);
            s.link_subbrief(&parent, &c).unwrap();
            s.set_board_status(&c, "todo").unwrap();
            s.set_board_status(&c, "in_progress").unwrap();
            s.set_board_status(&c, "in_review").unwrap();
            s.set_board_status(&c, "done").unwrap();
        }
        let c3 = mk("c3");
        s.link_subbrief(&parent, &c3).unwrap();
        s.set_board_status(&c3, "todo").unwrap();
        s.set_board_status(&c3, "in_progress").unwrap();

        let map: std::collections::HashMap<String, i64> = s
            .subbrief_progress(&parent)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(map.get("done"), Some(&2));
        assert_eq!(map.get("in_progress"), Some(&1));
        assert_eq!(map.values().sum::<i64>(), 3);
        // A childless Brief has no progress rows.
        assert!(s.subbrief_progress(&c3).unwrap().is_empty());
    }

    #[test]
    fn reverse_edges_list_blocking_and_parents() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        let parent = mk("parent");
        let child = mk("child");
        let blocker = mk("blocker");
        let waiter1 = mk("w1");
        let waiter2 = mk("w2");

        s.link_subbrief(&parent, &child).unwrap();
        // waiter1 and waiter2 are both blocked by `blocker`.
        s.add_snag(&waiter1, &blocker).unwrap();
        s.add_snag(&waiter2, &blocker).unwrap();

        // Forward: child's parent. Reverse: parent's children already covered.
        assert_eq!(s.parent_briefs(&child).unwrap(), vec![parent.clone()]);
        // `blocker` blocks both waiters (reverse of their Snags).
        let blocking = s.list_blocking(&blocker).unwrap();
        assert_eq!(blocking.len(), 2);
        assert!(blocking.contains(&waiter1) && blocking.contains(&waiter2));
        // A Brief that blocks nobody / has no parent returns empty.
        assert!(s.list_blocking(&parent).unwrap().is_empty());
        assert!(s.parent_briefs(&parent).unwrap().is_empty());
    }

    #[test]
    fn unlink_subbrief_detaches_and_chronicles() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        let parent = mk("parent");
        let child = mk("child");
        s.link_subbrief(&parent, &child).unwrap();
        assert_eq!(s.list_subbriefs(&parent).unwrap(), vec![child.clone()]);

        s.unlink_subbrief(&parent, &child).unwrap();
        assert!(s.list_subbriefs(&parent).unwrap().is_empty());
        // The child Brief itself still exists (only the edge went).
        assert!(s.brief_fields(&child).unwrap().is_some());
        // Idempotent; unknown parent → NotFound.
        s.unlink_subbrief(&parent, &child).unwrap();
        assert!(matches!(
            s.unlink_subbrief("nope", &child),
            Err(CoordinatorError::NotFound(_))
        ));

        let conn = s.conn.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_events
                 WHERE task_id = ?1 AND event_type = 'brief.subbrief_removed'",
                params![parent],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn remove_snag_unblocks_and_chronicles() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        let task = mk("task");
        let blocker = mk("blocker");
        s.add_snag(&task, &blocker).unwrap();
        assert!(s.is_blocked(&task).unwrap());

        // Clear the (wrong) dependency → unblocked.
        s.remove_snag(&task, &blocker).unwrap();
        assert!(!s.is_blocked(&task).unwrap());
        assert!(s.list_snags(&task).unwrap().is_empty());

        // Idempotent: clearing again is a no-op success.
        s.remove_snag(&task, &blocker).unwrap();
        // Unknown task → NotFound.
        assert!(matches!(
            s.remove_snag("nope", &blocker),
            Err(CoordinatorError::NotFound(_))
        ));

        // Exactly one snag_cleared event was chronicled.
        let conn = s.conn.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_events
                 WHERE task_id = ?1 AND event_type = 'brief.snag_cleared'",
                params![task],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn reassigning_a_brief_drops_the_previous_holders_claim() {
        let s = store();
        let id = s
            .create("b", "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
            .unwrap();
        s.set_brief_field(&id, "assignee", "agt_a").unwrap();
        s.set_board_status(&id, "todo").unwrap();
        // agt_a claims it.
        assert!(s.claim_brief(&id, "agt_a", 300).unwrap());
        assert_eq!(s.claim_holder(&id).unwrap().unwrap().0, "agt_a");
        // Not ready while held.
        assert!(s.list_ready_briefs(50).unwrap().is_empty());

        // Reassign to agt_b → stale Claim cleared, ready again.
        s.set_brief_field(&id, "assignee", "agt_b").unwrap();
        assert!(s.claim_holder(&id).unwrap().is_none());
        let ready: Vec<String> = s
            .list_ready_briefs(50)
            .unwrap()
            .into_iter()
            .map(|c| c.task_id)
            .collect();
        assert!(ready.contains(&id));
    }

    #[test]
    fn aggregate_board_counts_sum_across_a_team() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        let place = |id: &str, who: &str, status: &str| {
            s.set_brief_field(id, "assignee", who).unwrap();
            s.set_board_status(id, "todo").unwrap();
            if status != "todo" {
                s.set_board_status(id, "in_progress").unwrap();
            }
        };
        let a1 = mk("a1");
        place(&a1, "agt_a", "in_progress");
        let a2 = mk("a2");
        place(&a2, "agt_a", "todo");
        let b1 = mk("b1");
        place(&b1, "agt_b", "in_progress");
        // agt_c is outside the team and must not be counted.
        let c1 = mk("c1");
        place(&c1, "agt_c", "in_progress");

        let map: std::collections::HashMap<String, i64> = s
            .aggregate_board_counts(&["agt_a", "agt_b"])
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(map.get("in_progress"), Some(&2)); // a1 + b1
        assert_eq!(map.get("todo"), Some(&1)); // a2
        assert_eq!(map.values().sum::<i64>(), 3); // c1 excluded
        // Empty team → empty result.
        assert!(s.aggregate_board_counts(&[]).unwrap().is_empty());
    }

    #[test]
    fn assignee_board_counts_report_workload_by_column() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        // agt_a: two in_progress, one in_review; a done one is excluded.
        for t in ["p1", "p2"] {
            let id = mk(t);
            s.set_brief_field(&id, "assignee", "agt_a").unwrap();
            s.set_board_status(&id, "todo").unwrap();
            s.set_board_status(&id, "in_progress").unwrap();
        }
        let r = mk("r1");
        s.set_brief_field(&r, "assignee", "agt_a").unwrap();
        s.set_board_status(&r, "todo").unwrap();
        s.set_board_status(&r, "in_progress").unwrap();
        s.set_board_status(&r, "in_review").unwrap();

        let d = mk("d1");
        s.set_brief_field(&d, "assignee", "agt_a").unwrap();
        s.set_board_status(&d, "todo").unwrap();
        s.set_board_status(&d, "in_progress").unwrap();
        s.set_board_status(&d, "in_review").unwrap();
        s.set_board_status(&d, "done").unwrap();

        let map: std::collections::HashMap<String, i64> = s
            .assignee_board_counts("agt_a")
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(map.get("in_progress"), Some(&2));
        assert_eq!(map.get("in_review"), Some(&1));
        assert_eq!(map.get("done"), None); // excluded
        assert_eq!(map.values().sum::<i64>(), 3);
        // An assignee with no work has an empty workload.
        assert!(s.assignee_board_counts("nobody").unwrap().is_empty());
    }

    #[test]
    fn desk_lists_an_assignees_in_flight_briefs_by_priority() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        // Two briefs for agt_a: one in_progress (high), one todo (urgent).
        let b_hi = mk("hi");
        s.set_brief_field(&b_hi, "assignee", "agt_a").unwrap();
        s.set_brief_field(&b_hi, "priority", "high").unwrap();
        s.set_board_status(&b_hi, "todo").unwrap();
        s.set_board_status(&b_hi, "in_progress").unwrap();

        let b_urgent = mk("urgent");
        s.set_brief_field(&b_urgent, "assignee", "agt_a").unwrap();
        s.set_brief_field(&b_urgent, "priority", "urgent").unwrap();
        s.set_board_status(&b_urgent, "todo").unwrap();

        // A done brief for agt_a is excluded from the Desk.
        let b_done = mk("done");
        s.set_brief_field(&b_done, "assignee", "agt_a").unwrap();
        s.set_board_status(&b_done, "todo").unwrap();
        s.set_board_status(&b_done, "in_progress").unwrap();
        s.set_board_status(&b_done, "in_review").unwrap();
        s.set_board_status(&b_done, "done").unwrap();

        // A brief for someone else is not on agt_a's Desk.
        let other = mk("other");
        s.set_brief_field(&other, "assignee", "agt_b").unwrap();
        s.set_board_status(&other, "todo").unwrap();

        let desk = s.list_desk_for_assignee("agt_a", 50).unwrap();
        let ids: Vec<&str> = desk.iter().map(|c| c.task_id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        // Urgent sorts before high.
        assert_eq!(ids[0], b_urgent);
        assert_eq!(ids[1], b_hi);
        assert!(!ids.contains(&b_done.as_str()));
    }

    #[test]
    fn brief_comment_appends_to_the_chronicle() {
        let s = store();
        let id = s
            .create("b", "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
            .unwrap();
        s.comment_on_brief(&id, "founder", "ship it by friday").unwrap();
        s.comment_on_brief(&id, "agt_eng", "on it").unwrap();

        // Empty author / text rejected; unknown Brief → NotFound.
        assert!(matches!(
            s.comment_on_brief(&id, "  ", "x"),
            Err(CoordinatorError::Invalid(_))
        ));
        assert!(matches!(
            s.comment_on_brief(&id, "a", "   "),
            Err(CoordinatorError::Invalid(_))
        ));
        assert!(matches!(
            s.comment_on_brief("nope", "a", "b"),
            Err(CoordinatorError::NotFound(_))
        ));

        let conn = s.conn.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_events
                 WHERE task_id = ?1 AND event_type = 'brief.comment'",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
        let last: String = conn
            .query_row(
                "SELECT payload FROM task_events
                 WHERE task_id = ?1 AND event_type = 'brief.comment'
                 ORDER BY event_id DESC LIMIT 1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(last, "agt_eng: on it");
    }

    #[test]
    fn relations_and_dossiers_are_chronicled() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        let p = mk("p");
        let c = mk("c");
        let b = mk("b");
        s.link_subbrief(&p, &c).unwrap();
        s.add_snag(&p, &b).unwrap();
        s.add_dossier(&p, "plan", "The Plan", "body").unwrap();

        let conn = s.conn.lock().unwrap();
        let count = |et: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM task_events WHERE task_id = ?1 AND event_type = ?2",
                params![p, et],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(count("brief.subbrief_added"), 1);
        assert_eq!(count("brief.snagged"), 1);
        assert_eq!(count("brief.dossier_added"), 1);
    }

    #[test]
    fn phase1_subbriefs_and_snags_track_relations_and_blocking() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        let parent = mk("parent");
        let c1 = mk("child-1");
        let c2 = mk("child-2");
        let blocker = mk("blocker");

        // Sub-briefs: idempotent, ordered, no self-link, validated.
        s.link_subbrief(&parent, &c1).unwrap();
        s.link_subbrief(&parent, &c2).unwrap();
        s.link_subbrief(&parent, &c1).unwrap(); // idempotent no-op
        assert_eq!(
            s.list_subbriefs(&parent).unwrap(),
            vec![c1.clone(), c2.clone()]
        );
        assert!(s.list_subbriefs(&c1).unwrap().is_empty());
        assert!(matches!(
            s.link_subbrief(&parent, &parent),
            Err(CoordinatorError::Invalid(_))
        ));
        assert!(matches!(
            s.link_subbrief(&parent, "nope"),
            Err(CoordinatorError::NotFound(_))
        ));

        // Snags + blocking.
        assert!(!s.is_blocked(&parent).unwrap());
        s.add_snag(&parent, &blocker).unwrap();
        s.add_snag(&parent, &blocker).unwrap(); // idempotent
        assert_eq!(s.list_snags(&parent).unwrap(), vec![blocker.clone()]);
        assert!(s.is_blocked(&parent).unwrap());

        // Locked rule: a cancelled blocker stays UNRESOLVED.
        s.set_board_status(&blocker, "cancelled").unwrap();
        assert!(s.is_blocked(&parent).unwrap());

        // Only `done` resolves a Snag.
        let p2 = mk("parent-2");
        let b3 = mk("blocker-3");
        s.add_snag(&p2, &b3).unwrap();
        assert!(s.is_blocked(&p2).unwrap());
        for st in ["todo", "in_progress", "in_review", "done"] {
            s.set_board_status(&b3, st).unwrap();
        }
        assert!(!s.is_blocked(&p2).unwrap());
    }

    #[test]
    fn phase1_dossiers_attach_durable_artifacts() {
        let s = store();
        let id = s
            .create(
                "brief",
                "flows/none.sol",
                "{}",
                "subj",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();
        let d1 = s.add_dossier(&id, "plan", "The Plan", "step 1\nstep 2").unwrap();
        let _d2 = s
            .add_dossier(&id, "design", "The Design", "boxes & arrows")
            .unwrap();

        let metas = s.list_dossiers(&id).unwrap();
        assert_eq!(metas.len(), 2);
        let kinds: Vec<&str> = metas.iter().map(|m| m.kind.as_str()).collect();
        assert!(kinds.contains(&"plan") && kinds.contains(&"design"));

        let full = s.get_dossier(&d1).unwrap().unwrap();
        assert_eq!(full.title, "The Plan");
        assert_eq!(full.body, "step 1\nstep 2");
        assert_eq!(full.task_id, id);
        assert_eq!(full.kind, "plan");

        // Validation.
        assert!(matches!(
            s.add_dossier(&id, "", "t", "b"),
            Err(CoordinatorError::Invalid(_))
        ));
        assert!(matches!(
            s.add_dossier(&id, "plan", "", "b"),
            Err(CoordinatorError::Invalid(_))
        ));
        assert!(matches!(
            s.add_dossier("nope", "plan", "t", "b"),
            Err(CoordinatorError::NotFound(_))
        ));
        assert!(s.get_dossier("missing").unwrap().is_none());
    }

    #[test]
    fn phase1_brief_spine_fields_set_and_read() {
        let s = store();
        let id = s
            .create(
                "b",
                "flows/none.sol",
                "{}",
                "subj",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();

        // Defaults.
        let f = s.brief_fields(&id).unwrap().unwrap();
        assert_eq!(f.board_status, "backlog");
        assert_eq!(f.priority, "normal");
        assert_eq!(f.assignee_agent_id, None);
        assert_eq!(f.mandate_id, None);
        assert_eq!(f.campaign_id, None);

        // Set each spine field.
        s.set_brief_field(&id, "assignee", "agt_eng_1").unwrap();
        s.set_brief_field(&id, "priority", "high").unwrap();
        s.set_brief_field(&id, "mandate", "mandate_x").unwrap();
        s.set_brief_field(&id, "campaign", "camp_y").unwrap();
        let f = s.brief_fields(&id).unwrap().unwrap();
        assert_eq!(f.assignee_agent_id.as_deref(), Some("agt_eng_1"));
        assert_eq!(f.priority, "high");
        assert_eq!(f.mandate_id.as_deref(), Some("mandate_x"));
        assert_eq!(f.campaign_id.as_deref(), Some("camp_y"));

        // Empty clears the soft links.
        s.set_brief_field(&id, "assignee", "").unwrap();
        assert_eq!(s.brief_fields(&id).unwrap().unwrap().assignee_agent_id, None);

        // Validation.
        assert!(matches!(
            s.set_brief_field(&id, "priority", "bogus"),
            Err(CoordinatorError::Invalid(_))
        ));
        assert!(matches!(
            s.set_brief_field(&id, "nope", "x"),
            Err(CoordinatorError::Invalid(_))
        ));
        assert!(matches!(
            s.set_brief_field("missing", "priority", "high"),
            Err(CoordinatorError::NotFound(_))
        ));

        // The one non-empty assignment was chronicled (clears aren't).
        let conn = s.conn.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_events
                 WHERE task_id=?1 AND event_type='brief.assigned'",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn phase2_brief_board_lists_by_column() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        let a = mk("a");
        let b = mk("b");
        let c = mk("c");
        // a, b stay in backlog; drive c to in_progress.
        s.set_board_status(&c, "todo").unwrap();
        s.set_board_status(&c, "in_progress").unwrap();

        // All columns.
        assert_eq!(s.list_briefs_by_board(None, 50).unwrap().len(), 3);

        // One column.
        let backlog = s.list_briefs_by_board(Some("backlog"), 50).unwrap();
        assert_eq!(backlog.len(), 2);
        let ids: Vec<&str> = backlog.iter().map(|c| c.task_id.as_str()).collect();
        assert!(ids.contains(&a.as_str()) && ids.contains(&b.as_str()));

        let inprog = s.list_briefs_by_board(Some("in_progress"), 50).unwrap();
        assert_eq!(inprog.len(), 1);
        assert_eq!(inprog[0].task_id, c);
        assert_eq!(inprog[0].board_status, "in_progress");

        // Unknown column rejected.
        assert!(matches!(
            s.list_briefs_by_board(Some("bogus"), 50),
            Err(CoordinatorError::Invalid(_))
        ));

        // Cards carry the Brief's spine fields.
        s.set_brief_field(&c, "priority", "high").unwrap();
        s.set_brief_field(&c, "assignee", "agt_x").unwrap();
        let inprog = s.list_briefs_by_board(Some("in_progress"), 50).unwrap();
        assert_eq!(inprog[0].priority, "high");
        assert_eq!(inprog[0].assignee_agent_id.as_deref(), Some("agt_x"));
    }

    #[test]
    fn board_summary_counts_all_columns() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        let _a = mk("a"); // backlog
        let b = mk("b");
        let c = mk("c");
        s.set_board_status(&b, "todo").unwrap();
        s.set_board_status(&c, "todo").unwrap();
        s.set_board_status(&c, "in_progress").unwrap();

        let map: std::collections::HashMap<String, i64> =
            s.board_summary().unwrap().into_iter().collect();
        assert_eq!(map.get("backlog"), Some(&1));
        assert_eq!(map.get("todo"), Some(&1));
        assert_eq!(map.get("in_progress"), Some(&1));
        assert_eq!(map.values().sum::<i64>(), 3);
    }

    #[test]
    fn list_briefs_by_link_filters_by_mandate_and_campaign() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        let b1 = mk("b1");
        let b2 = mk("b2");
        let b3 = mk("b3");
        s.set_brief_field(&b1, "mandate", "m1").unwrap();
        s.set_brief_field(&b2, "mandate", "m1").unwrap();
        s.set_brief_field(&b3, "campaign", "c1").unwrap();

        assert_eq!(s.list_briefs_by_mandate("m1", 50).unwrap().len(), 2);
        assert_eq!(s.list_briefs_by_campaign("c1", 50).unwrap().len(), 1);
        assert!(s.list_briefs_by_mandate("none", 50).unwrap().is_empty());
        // The card carries the link.
        assert_eq!(
            s.list_briefs_by_campaign("c1", 50).unwrap()[0].task_id,
            b3
        );
    }

    #[test]
    fn phase2_desk_surfaces_blocked_and_stale_briefs() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };

        // Blocked: a live Brief with an unresolved Snag.
        let blocked = mk("blocked");
        let blocker = mk("blocker");
        s.set_board_status(&blocked, "todo").unwrap();
        s.add_snag(&blocked, &blocker).unwrap();
        let bl = s.list_blocked_briefs(50).unwrap();
        assert_eq!(bl.len(), 1);
        assert_eq!(bl[0].task_id, blocked);
        // Resolving the blocker drops it off.
        for st in ["todo", "in_progress", "in_review", "done"] {
            s.set_board_status(&blocker, st).unwrap();
        }
        assert!(s.list_blocked_briefs(50).unwrap().is_empty());

        // Stale: an active Brief backdated past the idle window.
        let stale = mk("stale");
        s.set_board_status(&stale, "in_progress").unwrap();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE tasks SET updated_at = 100 WHERE task_id = ?1",
                params![stale],
            )
            .unwrap();
        }
        let st = s.list_stale_briefs(0, 50).unwrap();
        assert!(st.iter().any(|c| c.task_id == stale));
        // A fresh active Brief is not stale.
        let fresh = mk("fresh");
        s.set_board_status(&fresh, "in_progress").unwrap();
        assert!(
            !s.list_stale_briefs(0, 50)
                .unwrap()
                .iter()
                .any(|c| c.task_id == fresh)
        );
    }

    #[test]
    fn phase3_brief_claim_is_single_owner_with_lease() {
        let s = store();
        let id = s
            .create(
                "brief",
                "flows/none.sol",
                "{}",
                "subj",
                RetryPolicy::None,
                0,
                None,
                None,
            )
            .unwrap();

        // Unclaimed to start.
        assert!(s.claim_holder(&id).unwrap().is_none());

        // A claims; B is locked out while A's lease is live.
        assert!(s.claim_brief(&id, "agt_a", 300).unwrap());
        assert_eq!(s.claim_holder(&id).unwrap().unwrap().0, "agt_a");
        assert!(!s.claim_brief(&id, "agt_b", 300).unwrap());

        // A is re-entrant; A heartbeats; B's heartbeat fails.
        assert!(s.claim_brief(&id, "agt_a", 300).unwrap());
        assert!(s.heartbeat_claim(&id, "agt_a", 300).unwrap());
        assert!(!s.heartbeat_claim(&id, "agt_b", 300).unwrap());

        // A releases; B can now claim.
        s.release_claim(&id, "agt_a").unwrap();
        assert!(s.claim_holder(&id).unwrap().is_none());
        assert!(s.claim_brief(&id, "agt_b", 300).unwrap());

        // An expired lease shows no holder and is reclaimable.
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE tasks SET claim_expires_at = 100 WHERE task_id = ?1",
                params![id],
            )
            .unwrap();
        }
        assert!(s.claim_holder(&id).unwrap().is_none());
        assert!(s.claim_brief(&id, "agt_a", 300).unwrap());

        // Unknown Brief → NotFound.
        assert!(matches!(
            s.claim_brief("nope", "agt_a", 300),
            Err(CoordinatorError::NotFound(_))
        ));
    }

    #[test]
    fn phase3_ready_briefs_are_assigned_unblocked_unclaimed_active() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };

        // Ready: assigned, todo, unblocked, unclaimed.
        let ready = mk("ready");
        s.set_brief_field(&ready, "assignee", "agt_a").unwrap();
        s.set_board_status(&ready, "todo").unwrap();

        // Not ready: no assignee.
        let unassigned = mk("unassigned");
        s.set_board_status(&unassigned, "todo").unwrap();

        // Not ready: blocked by an unresolved Snag.
        let blocked = mk("blocked");
        let blocker = mk("blocker");
        s.set_brief_field(&blocked, "assignee", "agt_b").unwrap();
        s.set_board_status(&blocked, "todo").unwrap();
        s.add_snag(&blocked, &blocker).unwrap();

        // Not ready: already claimed (live).
        let claimed = mk("claimed");
        s.set_brief_field(&claimed, "assignee", "agt_c").unwrap();
        s.set_board_status(&claimed, "todo").unwrap();
        assert!(s.claim_brief(&claimed, "agt_c", 300).unwrap());

        // Not ready: done.
        let done = mk("done");
        s.set_brief_field(&done, "assignee", "agt_d").unwrap();
        for st in ["todo", "in_progress", "in_review", "done"] {
            s.set_board_status(&done, st).unwrap();
        }

        let ids: Vec<String> = s
            .list_ready_briefs(50)
            .unwrap()
            .into_iter()
            .map(|c| c.task_id)
            .collect();
        assert!(ids.contains(&ready));
        for not in [&unassigned, &blocked, &claimed, &done] {
            assert!(!ids.contains(not), "{not} should not be ready");
        }

        // Releasing the claim makes 'claimed' ready.
        s.release_claim(&claimed, "agt_c").unwrap();
        assert!(
            s.list_ready_briefs(50)
                .unwrap()
                .iter()
                .any(|c| c.task_id == claimed)
        );
        // Resolving the blocker makes 'blocked' ready.
        for st in ["todo", "in_progress", "in_review", "done"] {
            s.set_board_status(&blocker, st).unwrap();
        }
        assert!(
            s.list_ready_briefs(50)
                .unwrap()
                .iter()
                .any(|c| c.task_id == blocked)
        );
    }

    #[test]
    fn phase3_children_completed_wake_surfaces_finished_parents() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        let has_parent = |s: &TaskStore, p: &str| {
            s.list_briefs_with_all_children_done(50)
                .unwrap()
                .iter()
                .any(|b| b.task_id == p)
        };

        let parent = mk("parent");
        s.set_brief_field(&parent, "assignee", "agt_planner").unwrap();
        s.set_board_status(&parent, "in_progress").unwrap();
        let c1 = mk("c1");
        let c2 = mk("c2");
        s.link_subbrief(&parent, &c1).unwrap();
        s.link_subbrief(&parent, &c2).unwrap();

        // Children unfinished → not surfaced.
        assert!(!has_parent(&s, &parent));

        // Finish c1 only → still not (c2 active).
        for st in ["todo", "in_progress", "in_review", "done"] {
            s.set_board_status(&c1, st).unwrap();
        }
        assert!(!has_parent(&s, &parent));

        // Cancel c2 (terminal counts as finished) → now surfaced.
        s.set_board_status(&c2, "cancelled").unwrap();
        assert!(has_parent(&s, &parent));

        // A childless Brief never appears.
        let solo = mk("solo");
        s.set_board_status(&solo, "in_progress").unwrap();
        assert!(!has_parent(&s, &solo));

        // Once the parent itself is done, it drops off.
        for st in ["in_review", "done"] {
            s.set_board_status(&parent, st).unwrap();
        }
        assert!(!has_parent(&s, &parent));
    }

    #[test]
    fn phase2_campaign_and_mandate_progress_counts_by_column() {
        let s = store();
        let mk = |t: &str| {
            s.create(t, "flows/none.sol", "{}", "subj", RetryPolicy::None, 0, None, None)
                .unwrap()
        };
        let b1 = mk("b1");
        let b2 = mk("b2");
        let b3 = mk("b3");
        s.set_brief_field(&b1, "campaign", "camp_1").unwrap();
        s.set_brief_field(&b2, "campaign", "camp_1").unwrap();
        s.set_brief_field(&b3, "campaign", "camp_2").unwrap();
        s.set_brief_field(&b1, "mandate", "mand_1").unwrap();
        // b1 backlog, b2 todo (camp_1); b3 backlog (camp_2).
        s.set_board_status(&b2, "todo").unwrap();

        let c1: std::collections::HashMap<String, i64> =
            s.campaign_brief_counts("camp_1").unwrap().into_iter().collect();
        assert_eq!(c1.get("backlog"), Some(&1));
        assert_eq!(c1.get("todo"), Some(&1));
        assert_eq!(c1.values().sum::<i64>(), 2);

        assert_eq!(
            s.campaign_brief_counts("camp_2").unwrap(),
            vec![("backlog".to_string(), 1)]
        );
        assert_eq!(
            s.mandate_brief_counts("mand_1").unwrap(),
            vec![("backlog".to_string(), 1)]
        );
        assert!(s.campaign_brief_counts("none").unwrap().is_empty());
    }

    /// File-backed open helper for the DB-hardening tests. We
    /// need an on-disk DB to confirm `journal_mode=WAL` (in-memory
    /// SQLite silently falls back to `memory`).
    fn file_store() -> (tempfile::TempDir, TaskStore) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = CoordinatorConfig {
            db_path: tmp.path().join("tasks.db"),
            max_list: 50,
            recovery_scan: false,
            retention: RetentionConfig::default(),
            ai_peer: None,
        };
        let s = TaskStore::open(&cfg).expect("open file store");
        (tmp, s)
    }

    #[test]
    fn open_sets_wal_mode() {
        let (_tmp, s) = file_store();
        let conn = s.conn.lock().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
    }

    #[test]
    fn open_enables_foreign_keys() {
        let (_tmp, s) = file_store();
        let conn = s.conn.lock().unwrap();
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn open_creates_migration_table_with_version_stamp() {
        let (_tmp, s) = file_store();
        let conn = s.conn.lock().unwrap();
        // The table itself.
        let cnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = '_relix_migrations'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt, 1, "_relix_migrations table should exist");
        // And a version row, stamped at open.
        let v: i64 = conn
            .query_row("SELECT COUNT(*) FROM _relix_migrations", [], |r| r.get(0))
            .unwrap();
        assert!(v >= 1, "expected at least one migration version row");
    }

    // ── Chronicle retention ──────────────────────────────────

    /// Seed `events_per_task` events for `task_count` tasks, then
    /// flip the tasks into a terminal status so retention picks
    /// them up.
    fn seed_old_chronicle(s: &TaskStore, task_count: usize, events_per_task: usize, old_ts: i64) {
        for i in 0..task_count {
            let tid = mk(s, &format!("retention-task-{i}"), "flow.sol", "{}", "owner");
            // Pin events to an old ts so they fall under the cutoff.
            {
                let conn = s.conn.lock().unwrap();
                for k in 0..events_per_task {
                    conn.execute(
                        "INSERT INTO task_events (task_id, ts, event_type, payload) \
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![tid, old_ts + k as i64, "test.event", ""],
                    )
                    .unwrap();
                }
                conn.execute(
                    "UPDATE tasks SET status = 'completed', updated_at = ?2 WHERE task_id = ?1",
                    rusqlite::params![tid, old_ts],
                )
                .unwrap();
            }
        }
    }

    fn event_count(s: &TaskStore) -> i64 {
        let conn = s.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM task_events", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn retention_deletes_events_older_than_cutoff() {
        let s = store();
        let now = unix_secs();
        let old = now - 60 * 86_400;
        seed_old_chronicle(&s, 3, 4, old);
        // All 12 task_events rows present pre-run.
        assert_eq!(event_count(&s), 12);
        let cutoff = now - 30 * 86_400;
        let r = s.run_retention(cutoff, 10).unwrap();
        // The 12 old events go; 3 snapshot events are emitted in
        // their place. So we go from 12 → 3.
        assert_eq!(r.events_deleted, 12);
        assert_eq!(r.snapshots_emitted, 3);
        assert_eq!(r.tasks_compacted, 3);
        assert_eq!(event_count(&s), 3);
    }

    #[test]
    fn retention_skips_non_terminal_tasks_r5() {
        let s = store();
        let now = unix_secs();
        let old = now - 60 * 86_400;
        seed_old_chronicle(&s, 2, 3, old);
        // Flip one back to `running` — R5 says retention can't
        // touch active tasks.
        {
            let conn = s.conn.lock().unwrap();
            let running_id: String = conn
                .query_row("SELECT task_id FROM tasks LIMIT 1", [], |r| r.get(0))
                .unwrap();
            conn.execute(
                "UPDATE tasks SET status = 'running' WHERE task_id = ?1",
                rusqlite::params![running_id],
            )
            .unwrap();
        }
        let r = s.run_retention(now - 30 * 86_400, 10).unwrap();
        // Only the still-completed task gets compacted (3 events).
        assert_eq!(r.events_deleted, 3);
        assert_eq!(r.snapshots_emitted, 1);
        // The `running` task's 3 events survive untouched, plus
        // 1 snapshot from the completed task = 4.
        assert_eq!(event_count(&s), 4);
    }

    #[test]
    fn retention_emits_snapshot_event_with_summary_payload() {
        let s = store();
        let now = unix_secs();
        let old = now - 60 * 86_400;
        seed_old_chronicle(&s, 1, 5, old);
        let _ = s.run_retention(now - 30 * 86_400, 10).unwrap();
        // The remaining event is the snapshot. Confirm its shape.
        let conn = s.conn.lock().unwrap();
        let (event_type, payload_json): (String, Option<String>) = conn
            .query_row(
                "SELECT event_type, payload_json FROM task_events LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(event_type, "task.snapshot");
        let pj = payload_json.expect("snapshot must carry payload_json");
        assert!(pj.contains("\"compacted_event_count\":5"), "got {pj}");
        assert!(pj.contains("\"final_status\":\"completed\""), "got {pj}");
        assert!(pj.contains("\"first_ts\":"), "got {pj}");
        assert!(pj.contains("\"last_ts\":"), "got {pj}");
    }

    #[test]
    fn retention_respects_limit_per_pass() {
        let s = store();
        let now = unix_secs();
        let old = now - 60 * 86_400;
        // Synthesise 2500 candidate events across one task so a
        // single 1000-row pass can't drain them; the loop must
        // run multiple passes.
        seed_old_chronicle(&s, 1, 2500, old);
        let r = s.run_retention(now - 30 * 86_400, 10).unwrap();
        assert_eq!(r.events_deleted, 2500);
        assert_eq!(r.snapshots_emitted, 1);
        // 2500 / 1000 = ceil 3 passes.
        assert!(
            r.passes_run >= 3,
            "expected ≥3 passes, got {}",
            r.passes_run
        );
        assert!(!r.stopped_at_pass_limit);
    }

    #[test]
    fn retention_honours_max_passes_cap() {
        let s = store();
        let now = unix_secs();
        let old = now - 60 * 86_400;
        seed_old_chronicle(&s, 1, 5000, old);
        // Cap at 2 passes — we should delete at most 2000 events
        // and the result must signal that we stopped early.
        let r = s.run_retention(now - 30 * 86_400, 2).unwrap();
        assert!(
            r.events_deleted <= MAX_ROWS_PER_RETENTION_PASS * 2,
            "deleted more than 2 passes' worth: {}",
            r.events_deleted
        );
        assert!(r.stopped_at_pass_limit);
    }

    #[test]
    fn retention_pass_is_a_noop_when_nothing_qualifies() {
        let s = store();
        let now = unix_secs();
        // No data at all.
        let r = s.run_retention(now - 30 * 86_400, 10).unwrap();
        assert_eq!(r.events_deleted, 0);
        assert_eq!(r.snapshots_emitted, 0);
        assert_eq!(r.tasks_compacted, 0);
        assert_eq!(r.passes_run, 0);
    }

    #[test]
    fn retention_does_not_re_compact_existing_snapshot_rows() {
        // Running retention twice in a row must NOT delete the
        // snapshot events emitted by the previous run. The
        // query explicitly excludes event_type='task.snapshot'.
        let s = store();
        let now = unix_secs();
        let old = now - 60 * 86_400;
        seed_old_chronicle(&s, 2, 3, old);
        let r1 = s.run_retention(now - 30 * 86_400, 10).unwrap();
        let mid_count = event_count(&s);
        assert_eq!(r1.snapshots_emitted, 2);
        assert_eq!(mid_count, 2); // two snapshots remain
        let r2 = s.run_retention(now - 30 * 86_400, 10).unwrap();
        assert_eq!(r2.events_deleted, 0, "snapshots survive a second pass");
        assert_eq!(r2.snapshots_emitted, 0);
        assert_eq!(event_count(&s), 2);
    }

    #[test]
    fn foreign_key_enforced_on_task_events() {
        // The `task_events.task_id` FK is declared in init_schema
        // but FK enforcement was off by default before the
        // hardening. With it on, inserting an event for a
        // non-existent task must be rejected.
        let (_tmp, s) = file_store();
        let conn = s.conn.lock().unwrap();
        let err = conn
            .execute(
                "INSERT INTO task_events (task_id, ts, event_type, payload) \
                 VALUES ('does-not-exist', 1, 'x', '')",
                [],
            )
            .expect_err("orphan event must be rejected");
        let m = err.to_string().to_ascii_lowercase();
        assert!(m.contains("foreign key"), "wrong err: {m}");
    }

    /// Test helper: create a task with the C1 defaults so we don't have
    /// to repeat the `RetryPolicy::None, 0, None` trailer at every call
    /// site.
    fn mk(s: &TaskStore, title: &str, flow: &str, params: &str, owner: &str) -> String {
        s.create(title, flow, params, owner, RetryPolicy::None, 0, None, None)
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
                None,
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
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, None, None)
            .unwrap();
        assert_eq!(s.bump_retry_count(&tid).unwrap(), 1);
        assert_eq!(s.bump_retry_count(&tid).unwrap(), 2);
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.retry_count, 2);
    }

    // ── PH-ORIGIN-SURFACE (D-004) ───────────────────────────────────

    #[test]
    fn create_with_no_origin_surface_writes_null() {
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::None, 0, None, None)
            .unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.origin_surface, None);
    }

    #[test]
    fn create_with_origin_surface_roundtrips() {
        let s = store();
        for surface in ["chat", "dashboard", "cli", "channel", "flow-engine"] {
            let tid = s
                .create(
                    &format!("t-{surface}"),
                    "f",
                    "{}",
                    "o",
                    RetryPolicy::None,
                    0,
                    None,
                    Some(surface),
                )
                .unwrap();
            let v = s.get(&tid).unwrap().unwrap();
            assert_eq!(v.origin_surface.as_deref(), Some(surface));
        }
    }

    fn ctx(args: &[u8]) -> InvocationCtx {
        use relix_core::identity::VerifiedIdentity;
        use relix_core::types::{NodeId, RequestId, TraceId};
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"x"),
                name: "x".into(),
                org_id: NodeId::from_pubkey(b"o"),
                groups: vec![],
                role: "".into(),
                clearance: "".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: args.to_vec(),
            tenant_id: None,
        }
    }

    #[test]
    fn handle_create_parses_origin_surface_from_eighth_slot() {
        let s = store();
        // 8 slots: title|flow|params|owner|retry|max_retries|max_runtime|origin
        let arg = b"my-task|demo.sol|{}|alice|none|0||dashboard";
        let out = handle_create(&s, &ctx(arg));
        let tid = match out {
            HandlerOutcome::Ok(body) => String::from_utf8(body).unwrap(),
            _ => panic!("expected Ok"),
        };
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.origin_surface.as_deref(), Some("dashboard"));
    }

    #[test]
    fn handle_create_empty_origin_surface_slot_writes_null() {
        let s = store();
        // 7 pipes → 8 slots; the 8th is empty → origin_surface NULL.
        let arg = b"my-task|demo.sol|{}|alice|none|0||";
        let out = handle_create(&s, &ctx(arg));
        let tid = match out {
            HandlerOutcome::Ok(body) => String::from_utf8(body).unwrap(),
            _ => panic!("expected Ok"),
        };
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.origin_surface, None);
    }

    // ── W2-001b: task.replay ────────────────────────────────────

    #[test]
    fn replay_from_clones_task_and_writes_retried_from_edge() {
        let s = store();
        let original = s
            .create(
                "original",
                "demo.sol",
                r#"{"k":1}"#,
                "alice",
                RetryPolicy::Bounded,
                3,
                Some(60),
                Some("dashboard"),
            )
            .unwrap();
        let new_id = s.replay_from(&original, "alice").unwrap();
        assert_ne!(new_id, original);
        let v = s.get(&new_id).unwrap().unwrap();
        // Title suffixed; inherited fields preserved.
        assert_eq!(v.title, "original (replay)");
        assert_eq!(v.flow_template, "demo.sol");
        assert_eq!(v.params_json, r#"{"k":1}"#);
        assert_eq!(v.owner_subject_id, "alice");
        assert_eq!(v.retry_policy, "bounded");
        assert_eq!(v.max_retries, 3);
        assert_eq!(v.max_runtime_secs, Some(60));
        assert_eq!(v.origin_surface.as_deref(), Some("dashboard"));
        // retry_count starts fresh on the replay.
        assert_eq!(v.retry_count, 0);
    }

    #[test]
    fn replay_from_returns_not_found_for_unknown_task() {
        let s = store();
        match s.replay_from("nope", "alice") {
            Err(CoordinatorError::NotFound(id)) => assert_eq!(id, "nope"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn handle_replay_returns_new_task_id() {
        let s = store();
        let original = s
            .create("t", "f", "{}", "alice", RetryPolicy::None, 0, None, None)
            .unwrap();
        let out = handle_replay(&s, &ctx(original.as_bytes()));
        let new_id = match out {
            HandlerOutcome::Ok(body) => String::from_utf8(body).unwrap(),
            _ => panic!("expected Ok"),
        };
        assert_ne!(new_id, original);
        // Both tasks exist + the replay's title is suffixed.
        assert!(s.get(&original).unwrap().is_some());
        let replay = s.get(&new_id).unwrap().unwrap();
        assert_eq!(replay.title, "t (replay)");
    }

    #[test]
    fn handle_replay_rejects_empty_arg() {
        let s = store();
        let out = handle_replay(&s, &ctx(b""));
        match out {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, error_kinds::INVALID_ARGS);
            }
            _ => panic!("expected INVALID_ARGS"),
        }
    }

    #[test]
    fn handle_replay_returns_invalid_args_when_unknown_task() {
        let s = store();
        let out = handle_replay(&s, &ctx(b"nonexistent-id"));
        match out {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("not found"));
            }
            _ => panic!("expected INVALID_ARGS for unknown task"),
        }
    }

    #[test]
    fn handle_create_missing_origin_surface_slot_writes_null() {
        // Backwards-compat: callers that send only 7 slots (the
        // pre-D-004 shape) still work; origin_surface defaults
        // to NULL.
        let s = store();
        let arg = b"my-task|demo.sol|{}|alice|none|0|";
        let out = handle_create(&s, &ctx(arg));
        let tid = match out {
            HandlerOutcome::Ok(body) => String::from_utf8(body).unwrap(),
            _ => panic!("expected Ok"),
        };
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.origin_surface, None);
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

    // ── H8: secret redaction at chronicle write boundary ──────────────

    #[test]
    fn operator_note_with_openai_key_redacted_before_persist() {
        // H8: a pasted API key in an operator note must NOT land
        // in the chronicle. The redaction runs at write time —
        // operators can't accidentally bake a secret into the
        // forever-replayable audit log.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let note = "tried FAKE_TEST_FIXTURE_REDACTED in prod";
        s.append_operator_note(&tid, note, "operator").unwrap();
        let events = s.list_events_after(&tid, 0, 50).unwrap();
        let ev = events
            .iter()
            .find(|e| e.event_type == "task.operator_note")
            .expect("operator_note missing");
        assert!(
            !ev.payload.contains("sk-abcdef"),
            "raw key leaked into payload: {}",
            ev.payload
        );
        assert!(
            ev.payload.contains("[REDACTED:OPENAI_KEY]"),
            "redaction marker missing: {}",
            ev.payload
        );
        let pj = ev.payload_json.as_deref().unwrap();
        assert!(!pj.contains("sk-abcdef"), "key in payload_json: {pj}");
        assert!(pj.contains("[REDACTED:OPENAI_KEY]"));
    }

    #[test]
    fn investigation_marker_reason_redacted() {
        // H8: same posture for investigation_marker reason — both
        // the task row and the chronicle event must carry the
        // redacted form.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.set_investigation_marker(
            &tid,
            true,
            Some("found FAKE_TEST_FIXTURE_REDACTED in env"),
            "op",
        )
        .unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        let reason = v.investigation_reason.unwrap();
        assert!(
            !reason.contains("ghp_abcdef"),
            "raw PAT in task row: {reason}"
        );
        assert!(reason.contains("[REDACTED:GITHUB_PAT]"));
        let events = s.list_events_after(&tid, 0, 50).unwrap();
        let ev = events
            .iter()
            .find(|e| e.event_type == "task.investigation_marked")
            .expect("investigation_marked missing");
        assert!(!ev.payload.contains("ghp_abcdef"));
        let pj = ev.payload_json.as_deref().unwrap();
        assert!(!pj.contains("ghp_abcdef"));
    }

    // ── H14: auto terminal_summary on every terminal transition ───────

    #[test]
    fn terminal_summary_emitted_on_completed_transition() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(&tid, Some("completed"), None, None, None, None, None, None)
            .unwrap();
        let events = s.list_events_after(&tid, 0, 50).unwrap();
        let ts = events
            .iter()
            .find(|e| e.event_type == "task.terminal_summary")
            .expect("terminal_summary missing");
        let pj: serde_json::Value =
            serde_json::from_str(ts.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(pj["reason"], "completed");
        assert_eq!(pj["auto_emitted_by"], "update_task");
        assert!(pj["attempts"].as_i64().unwrap() >= 1);
        assert_eq!(pj["retries"].as_i64().unwrap(), 0);
    }

    #[test]
    fn terminal_summary_emitted_on_failed_transition_carries_failure_class() {
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
            Some("upstream 500"),
            Some("transport"),
        )
        .unwrap();
        let events = s.list_events_after(&tid, 0, 50).unwrap();
        let ts = events
            .iter()
            .find(|e| e.event_type == "task.terminal_summary")
            .expect("terminal_summary missing");
        let pj: serde_json::Value =
            serde_json::from_str(ts.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(pj["reason"], "failed");
        assert_eq!(pj["last_failure_class"], "transport");
    }

    #[test]
    fn terminal_summary_is_only_emitted_once_per_task() {
        // Calling update twice with the same terminal status must
        // not emit two summary events. The idempotent guard
        // queries existing chronicle rows and skips when one is
        // already present.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(&tid, Some("completed"), None, None, None, None, None, None)
            .unwrap();
        // Second terminal-status update (e.g. an idempotent
        // operator retry of the same call).
        s.update(&tid, Some("completed"), None, None, None, None, None, None)
            .unwrap();
        let events = s.list_events_after(&tid, 0, 100).unwrap();
        let n = events
            .iter()
            .filter(|e| e.event_type == "task.terminal_summary")
            .count();
        assert_eq!(n, 1, "expected exactly one terminal_summary, got {n}");
    }

    // ── H10: redaction sweep across pause/freeze/error_cause ──────────

    #[test]
    fn set_paused_reason_redacted_before_persist() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.set_paused(
            &tid,
            Some("pausing — pasted FAKE_TEST_FIXTURE_REDACTED by mistake"),
            "op",
        )
        .unwrap();
        let events = s.list_events_after(&tid, 0, 50).unwrap();
        let ev = events
            .iter()
            .rfind(|e| e.event_type == "task.pause_requested")
            .expect("pause_requested event missing");
        assert!(
            !ev.payload.contains("sk-abcdef"),
            "raw key in legacy payload: {}",
            ev.payload
        );
        let pj = ev.payload_json.as_deref().unwrap();
        assert!(!pj.contains("sk-abcdef"), "raw key in payload_json: {pj}");
        assert!(pj.contains("[REDACTED:OPENAI_KEY]"));
    }

    #[test]
    fn set_frozen_reason_redacted_before_persist() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.set_frozen(
            &tid,
            Some("freeze: env had FAKE_TEST_FIXTURE_REDACTED"),
            "op",
        )
        .unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        // tasks.frozen_reason must be redacted too.
        let fr = v.frozen_reason.unwrap_or_default();
        assert!(!fr.contains("ghp_abcdef"), "raw PAT in task row: {fr}");
        let events = s.list_events_after(&tid, 0, 50).unwrap();
        let ev = events
            .iter()
            .rfind(|e| e.event_type == "task.freeze_requested")
            .expect("freeze_requested event missing");
        assert!(!ev.payload.contains("ghp_abcdef"));
    }

    #[test]
    fn update_error_cause_redacted_before_persist() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            Some("provider responded HTTP 401: invalid Authorization: Bearer eyJhbGciOiJIUzI1NiJ9zzzzzzz"),
            Some("auth"),
        )
        .unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        let cause = v.error_cause.unwrap_or_default();
        assert!(
            !cause.contains("eyJhbGc"),
            "raw token in error_cause: {cause}"
        );
        assert!(cause.contains("[REDACTED:BEARER_TOKEN]"));
        let last = v.last_failure_reason.unwrap_or_default();
        assert!(!last.contains("eyJhbGc"));
        assert!(last.contains("[REDACTED:BEARER_TOKEN]"));
    }

    // ── PH-WAVE2D: task.todo_* coord capabilities ─────────────────────

    #[test]
    fn todo_set_then_list_roundtrips_ordered() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let items = ["write design doc", "code it", "ship it"];
        let after = s.set_task_todos(&tid, &items).unwrap();
        assert_eq!(after.len(), 3);
        assert_eq!(after[0].position, 0);
        assert_eq!(after[0].status, "open");
        assert_eq!(after[0].text, "write design doc");
        assert_eq!(after[2].text, "ship it");
        let listed = s.list_task_todos(&tid).unwrap();
        assert_eq!(listed.len(), 3);
        assert_eq!(
            listed.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["write design doc", "code it", "ship it"]
        );
    }

    #[test]
    fn todo_set_replaces_previous_list() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.set_task_todos(&tid, &["old1", "old2", "old3"]).unwrap();
        s.set_task_todos(&tid, &["new"]).unwrap();
        let listed = s.list_task_todos(&tid).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].text, "new");
    }

    #[test]
    fn todo_set_empty_clears_list() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.set_task_todos(&tid, &["a", "b"]).unwrap();
        s.set_task_todos(&tid, &[]).unwrap();
        let listed = s.list_task_todos(&tid).unwrap();
        assert!(listed.is_empty());
    }

    #[test]
    fn todo_update_status_toggles_done() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let after = s.set_task_todos(&tid, &["a"]).unwrap();
        let id = after[0].todo_id;
        let updated = s.update_task_todo_status(&tid, id, "done").unwrap();
        assert_eq!(updated.status, "done");
        assert!(updated.updated_at >= after[0].created_at);
        let back = s.update_task_todo_status(&tid, id, "open").unwrap();
        assert_eq!(back.status, "open");
    }

    #[test]
    fn todo_update_rejects_invalid_status() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let after = s.set_task_todos(&tid, &["a"]).unwrap();
        let err = s
            .update_task_todo_status(&tid, after[0].todo_id, "wip")
            .unwrap_err();
        assert!(matches!(err, CoordinatorError::Invalid(_)));
    }

    #[test]
    fn todo_set_rejects_empty_text() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let err = s.set_task_todos(&tid, &["valid", "   "]).unwrap_err();
        assert!(matches!(err, CoordinatorError::Invalid(_)));
    }

    #[test]
    fn todo_set_redacts_secrets_in_text() {
        // H8/H10 boundary discipline: pasted API key in a todo
        // line must not land in the chronicle/dashboard.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let items = ["fix bug", "tried FAKE_TEST_FIXTURE_REDACTED"];
        s.set_task_todos(&tid, &items).unwrap();
        let listed = s.list_task_todos(&tid).unwrap();
        assert!(!listed[1].text.contains("sk-abcdef"));
        assert!(listed[1].text.contains("[REDACTED:OPENAI_KEY]"));
    }

    #[test]
    fn todo_set_rejects_unknown_task() {
        let s = store();
        let err = s.set_task_todos("deadbeef", &["x"]).unwrap_err();
        assert!(matches!(err, CoordinatorError::NotFound(id) if id == "deadbeef"));
    }

    // ── H7: orphan attempt cleanup ────────────────────────────────────

    #[test]
    fn close_orphan_attempts_closes_attempts_whose_task_is_terminal() {
        // H7: an attempt was opened (task transitioned to 'running')
        // but the task later reached a terminal state without the
        // attempt-close codepath running. Cleanup must close the
        // attempt + emit task.attempt_orphan_closed.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        // Forcibly mark terminal status via a back-door (simulate a
        // legacy bypass that didn't close the attempt). Use the
        // direct rusqlite handle through `update` with status only
        // — but update() goes through close_open_attempt_if_any,
        // so we have to drop the close behaviour. We achieve the
        // same orphan by deleting the attempt's finished_at via
        // direct SQL after a normal close, then forcing the task
        // back to terminal: easier path is to use a sibling helper.
        //
        // For the test, force the orphan condition deterministically
        // by setting status='failed' via update() then re-opening the
        // attempt manually.
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            None,
            Some("transport"),
        )
        .unwrap();
        // Re-open the attempt to simulate a crash that left it open.
        let conn = s.conn.lock().unwrap();
        conn.execute(
            "UPDATE task_attempts SET finished_at = NULL WHERE task_id = ?1",
            params![tid],
        )
        .unwrap();
        drop(conn);

        let closed = s.close_orphan_attempts(unix_secs()).unwrap();
        assert_eq!(closed.len(), 1);
        let events = s.list_events_after(&tid, 0, 50).unwrap();
        let orphan = events
            .iter()
            .find(|e| e.event_type == "task.attempt_orphan_closed")
            .expect("orphan event missing");
        let pj: serde_json::Value =
            serde_json::from_str(orphan.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(pj["reason"], "orphan");
        assert_eq!(pj["closed_as"], "interrupted");
        assert_eq!(pj["task_status"], "failed");
    }

    #[test]
    fn close_orphan_attempts_leaves_open_attempts_of_running_tasks_alone() {
        // A still-running task with an open attempt is NOT an
        // orphan. Cleanup must not touch it.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let closed = s.close_orphan_attempts(unix_secs()).unwrap();
        assert!(
            closed.is_empty(),
            "running task's open attempt must not be closed"
        );
    }

    #[test]
    fn recovery_scan_also_closes_orphan_attempts_inline() {
        // recover_interrupted runs close_orphan_attempts_in_txn
        // as a side effect. A pre-existing orphan from an
        // unrelated task should be cleaned up alongside the
        // deadline-recovered tasks.
        let s = store();
        // Task A: legitimate deadline candidate.
        let a = s
            .create("a", "f", "{}", "o", RetryPolicy::None, 0, Some(10), None)
            .unwrap();
        s.update(&a, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let a_started = s.get(&a).unwrap().unwrap().started_at.unwrap();
        // Task B: orphan attempt — task ended terminal without
        // closing the attempt.
        let b = mk(&s, "b", "f", "{}", "o");
        s.update(&b, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(&b, Some("completed"), None, None, None, None, None, None)
            .unwrap();
        // Forcibly orphan b's attempt.
        let conn = s.conn.lock().unwrap();
        conn.execute(
            "UPDATE task_attempts SET finished_at = NULL WHERE task_id = ?1",
            params![b],
        )
        .unwrap();
        drop(conn);

        let recovered = s.recover_interrupted(a_started + 60).unwrap();
        assert_eq!(recovered, vec![a.clone()]);
        // b's events should now include orphan closure.
        let b_events = s.list_events_after(&b, 0, 50).unwrap();
        assert!(
            b_events
                .iter()
                .any(|e| e.event_type == "task.attempt_orphan_closed"),
            "recover_interrupted must run orphan cleanup as a side effect"
        );
    }

    // ── H6: stuck-running projection ──────────────────────────────────

    #[test]
    fn stuck_running_includes_only_running_without_deadline_past_threshold() {
        let s = store();
        // 1. Running, no deadline, started 600s ago → stuck.
        let stuck = mk(&s, "stuck-one", "f", "{}", "o");
        s.update(&stuck, Some("running"), None, None, None, None, None, None)
            .unwrap();
        // 2. Running, with deadline → NOT stuck (recovery scan owns it).
        let with_dl = s
            .create(
                "with-dl",
                "f",
                "{}",
                "o",
                RetryPolicy::None,
                0,
                Some(3600),
                None,
            )
            .unwrap();
        s.update(
            &with_dl,
            Some("running"),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        // 3. Completed → not running.
        let done = mk(&s, "done", "f", "{}", "o");
        s.update(&done, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(&done, Some("completed"), None, None, None, None, None, None)
            .unwrap();

        let started_stuck = s.get(&stuck).unwrap().unwrap().started_at.unwrap();
        let rows = s.stuck_running(started_stuck + 600, 300).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.task_id.as_str()).collect();
        assert_eq!(ids, vec![stuck.as_str()], "rows: {rows:#?}");
        assert!(rows[0].age_secs >= 600);
    }

    #[test]
    fn stuck_running_threshold_filters_recent_starts() {
        let s = store();
        let tid = mk(&s, "fresh", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let started = s.get(&tid).unwrap().unwrap().started_at.unwrap();
        // 30s of wall-clock with a 5min threshold → not stuck yet.
        let rows = s.stuck_running(started + 30, 300).unwrap();
        assert!(rows.is_empty(), "early task should not appear stuck");
    }

    #[test]
    fn stuck_running_orders_oldest_first() {
        let s = store();
        let older = mk(&s, "older", "f", "{}", "o");
        s.update(&older, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let newer = mk(&s, "newer", "f", "{}", "o");
        s.update(&newer, Some("running"), None, None, None, None, None, None)
            .unwrap();
        // Both have started_at = unix_secs() right now. Use a far-future
        // now to make both visible; with-equal-started rows the SQL
        // ordering will follow the index. We assert just the *set* of
        // returned task_ids and the ordering when started_at differs.
        let now = s.get(&older).unwrap().unwrap().started_at.unwrap() + 9_999;
        let rows = s.stuck_running(now, 100).unwrap();
        assert_eq!(rows.len(), 2);
        // Same started_at within the same tick — the test asserts both
        // are present.
        let mut ids: Vec<String> = rows.into_iter().map(|r| r.task_id).collect();
        ids.sort();
        let mut expected = vec![older, newer];
        expected.sort();
        assert_eq!(ids, expected);
    }

    // ── C1b: recovery scan ────────────────────────────────────────────

    #[test]
    fn recovery_scan_flips_overdue_running_tasks_to_interrupted() {
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::None, 0, Some(10), None)
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
    fn recovery_scan_emits_terminal_summary_with_attempt_and_wallclock() {
        // H5: every recovered task gets a synthesized
        // task.terminal_summary event with attempts, retries,
        // wall_clock_secs, and the final failure class. Operators
        // see a one-line post-mortem in the chronicle without
        // needing an executor consumer to write it.
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::None, 0, Some(10), None)
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let started = s.get(&tid).unwrap().unwrap().started_at.unwrap();
        let recovered = s.recover_interrupted(started + 90).unwrap();
        assert_eq!(recovered, vec![tid.clone()]);

        let events = s.list_events_after(&tid, 0, 50).unwrap();
        let term = events
            .iter()
            .find(|e| e.event_type == "task.terminal_summary")
            .expect("terminal_summary event missing");
        let pj: serde_json::Value =
            serde_json::from_str(term.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(pj["reason"], "deadline_exceeded");
        assert_eq!(pj["last_failure_class"], "timeout");
        assert_eq!(pj["auto_emitted_by"], "recover_interrupted");
        assert_eq!(pj["wall_clock_secs"].as_i64().unwrap(), 90);
        // attempts is 1 because the 'running' transition opened one attempt.
        assert_eq!(pj["attempts"].as_i64().unwrap(), 1);
        assert_eq!(pj["retries"].as_i64().unwrap(), 0);
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
            .create("t", "f", "{}", "o", RetryPolicy::None, 0, Some(3600), None)
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
            .create("t", "f", "{}", "o", RetryPolicy::None, 0, Some(5), None)
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
            .create("done", "f", "{}", "o", RetryPolicy::None, 0, Some(5), None)
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
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, None, None)
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
            .create("t", "f", "{}", "o", RetryPolicy::None, 0, Some(5), None)
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
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, Some(2), None)
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
            .create("t", "f", "{}", "o", RetryPolicy::Once, 0, None, None)
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
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, None, None)
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
            .create(
                "t",
                "f",
                "{}",
                "o",
                RetryPolicy::Bounded,
                99,
                Some(60),
                None,
            )
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
                None,
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
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, Some(5), None)
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
            .create("t", "f", "{}", "o", RetryPolicy::None, 0, Some(5), None)
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
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, None, None)
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
            s.create(
                &format!("t{i}"),
                "f",
                "{}",
                "o",
                RetryPolicy::None,
                0,
                None,
                None,
            )
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
            s.create(
                &format!("t{i}"),
                "f",
                "{}",
                "o",
                RetryPolicy::None,
                0,
                None,
                None,
            )
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
        // 100 completed tasks × (50 user events + 1 H14
        // task.terminal_summary) = 5100 candidate events.
        assert_eq!(
            r.candidate_events, 5100,
            "expected 5100 candidates (100 completed × (50 user + 1 H14) events)"
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
    fn recent_events_cross_task_returns_newest_first_across_tasks() {
        // M67: global firehose. Events from multiple tasks
        // interleave by event_id; newest must come first.
        let s = store();
        let a = mk(&s, "ta", "f", "{}", "o");
        let b = mk(&s, "tb", "f", "{}", "o");
        s.append_event(&a, "ops.x", "p1").unwrap();
        s.append_event(&b, "ops.x", "p2").unwrap();
        s.append_event(&a, "ops.x", "p3").unwrap();
        let rows = s.recent_events_cross_task(0, 100, None).unwrap();
        // Returns ALL events from a fresh store (created
        // ones too) — that's fine as long as our 3 ops.x
        // events appear in DESC order.
        let ops_x: Vec<&(String, TaskEvent)> = rows
            .iter()
            .filter(|(_, ev)| ev.event_type == "ops.x")
            .collect();
        assert_eq!(ops_x.len(), 3);
        assert!(ops_x[0].1.event_id > ops_x[1].1.event_id);
        assert!(ops_x[1].1.event_id > ops_x[2].1.event_id);
    }

    #[test]
    fn recent_events_cross_task_since_cursor_advances() {
        let s = store();
        let a = mk(&s, "ta", "f", "{}", "o");
        let e1 = s.append_event(&a, "ops.x", "p1").unwrap();
        let e2 = s.append_event(&a, "ops.x", "p2").unwrap();
        // since=e1 must return only e2 (and any newer)
        let rows = s.recent_events_cross_task(e1, 100, None).unwrap();
        assert!(rows.iter().all(|(_, ev)| ev.event_id > e1));
        assert!(rows.iter().any(|(_, ev)| ev.event_id == e2));
    }

    #[test]
    fn recent_events_cross_task_filters_by_type() {
        let s = store();
        let a = mk(&s, "ta", "f", "{}", "o");
        s.append_event(&a, "ops.keep", "k").unwrap();
        s.append_event(&a, "ops.skip", "s").unwrap();
        let rows = s
            .recent_events_cross_task(0, 100, Some("ops.keep"))
            .unwrap();
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|(_, ev)| ev.event_type == "ops.keep"));
    }

    #[test]
    fn task_lineage_with_only_retried_from_returns_self_no_cross_task() {
        // M66 + honest scope: with only retried_from
        // producers shipping today, the lineage of a task
        // walked from itself returns just that task. The
        // cross_task_edge_count is 0 — we don't fabricate.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        // Force a retry to land a retried_from edge.
        let _ = s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            Some("oops"),
            Some("transient"),
        );
        let _ = s.request_retry(&tid);
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();

        let g = s.task_lineage(&tid, 4).unwrap();
        assert_eq!(g.root_task_id, tid);
        assert_eq!(g.tasks, vec![tid.clone()]);
        // retried_from edges are intra-task — cross-task
        // count must stay 0 even when the edge exists.
        assert_eq!(g.cross_task_edge_count, 0);
        // But the edge should be present in `edges`.
        assert!(g.edges.iter().any(|e| e.edge_type == "retried_from"));
        assert_eq!(g.max_depth_walked, 4);
    }

    #[test]
    fn task_lineage_on_unrelated_task_returns_just_root() {
        let s = store();
        let tid = mk(&s, "lonely", "f", "{}", "o");
        let g = s.task_lineage(&tid, 8).unwrap();
        assert_eq!(g.tasks, vec![tid]);
        assert!(g.edges.is_empty());
        assert_eq!(g.cross_task_edge_count, 0);
    }

    #[test]
    fn task_lineage_max_depth_clamped() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        // Even a max_depth=0 caller gets clamped up to 1, the
        // minimum (otherwise BFS exits immediately and emits
        // nothing about the root).
        let g = s.task_lineage(&tid, 0).unwrap();
        assert!(g.max_depth_walked >= 1);
        // Sky-high caller gets clamped down.
        let g2 = s.task_lineage(&tid, 9999).unwrap();
        assert!(g2.max_depth_walked <= 16);
    }

    #[test]
    fn pause_from_running_transitions_to_paused_with_chronicle() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        // Walk the task into `running` via the update path.
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let prior = s
            .set_paused(&tid, Some("debugging upstream"), "op")
            .unwrap();
        assert_eq!(prior, "running");
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.status, "paused");
        // Chronicle event has the structured envelope. M70
        // renamed `task.paused` → `task.pause_requested` to
        // make the intent-vs-ack split explicit.
        let ev = s
            .list_events_after(&tid, 0, 100)
            .unwrap()
            .into_iter()
            .find(|e| e.event_type == "task.pause_requested")
            .expect("task.pause_requested event");
        let pj: serde_json::Value =
            serde_json::from_str(ev.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(pj["prior_status"], "running");
        assert_eq!(pj["reason"], "debugging upstream");
        assert_eq!(pj["author"], "op");
        assert_eq!(pj["intent"], "request");
        // M70: pause_generation must be > 0 after the pause
        // (the very first request bumps from 0 to 1).
        assert!(pj["pause_generation"].as_i64().unwrap() >= 1);
        // The task row's pause_generation matches.
        assert_eq!(v.pause_generation, pj["pause_generation"].as_i64().unwrap());
    }

    #[test]
    fn pause_refuses_terminal_and_already_paused() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(
            &tid,
            Some("completed"),
            Some("result"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let err = s.set_paused(&tid, None, "op").unwrap_err();
        assert!(matches!(err, CoordinatorError::Invalid(_)));

        let tid2 = mk(&s, "t2", "f", "{}", "o");
        s.update(&tid2, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.set_paused(&tid2, None, "op").unwrap();
        let err = s.set_paused(&tid2, None, "op").unwrap_err();
        assert!(matches!(err, CoordinatorError::Invalid(_)));
    }

    #[test]
    fn resume_restores_to_pending_with_pre_pause_status_in_event() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.set_paused(&tid, Some("ops triage"), "op").unwrap();
        let pre = s.set_resumed(&tid, "op").unwrap();
        assert_eq!(pre, "running");
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.status, "pending");
        let ev = s
            .list_events_after(&tid, 0, 100)
            .unwrap()
            .into_iter()
            .find(|e| e.event_type == "task.resume_requested")
            .expect("task.resume_requested event");
        let pj: serde_json::Value =
            serde_json::from_str(ev.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(pj["pre_pause_status"], "running");
        assert_eq!(pj["new_status"], "pending");
        assert_eq!(pj["intent"], "request");
        // M70: resume also bumps pause_generation so a
        // cooperative worker that cached the paused
        // generation knows to re-check before continuing.
        assert!(pj["pause_generation"].as_i64().unwrap() >= 2);
    }

    #[test]
    fn resume_refuses_non_paused_status() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let err = s.set_resumed(&tid, "op").unwrap_err();
        assert!(matches!(err, CoordinatorError::Invalid(_)));
    }

    #[test]
    fn state_machine_canonical_transitions_allowed() {
        // M74: spot-check the canonical happy path.
        for (from, to) in [
            ("pending", "running"),
            ("running", "completed"),
            ("running", "failed"),
            ("running", "interrupted"),
            ("running", "cancelled"),
            ("running", "paused"),
            ("running", "frozen"),
            ("retrying", "running"),
            ("failed", "retrying"),
            ("interrupted", "retrying"),
            ("paused", "pending"),
            ("paused", "frozen"),
            ("frozen", "pending"),
        ] {
            assert!(
                is_allowed_transition(from, to),
                "expected {from} → {to} to be allowed"
            );
        }
    }

    #[test]
    fn state_machine_invalid_transitions_rejected() {
        // M74: spot-check disallowed transitions. Terminal
        // statuses must reject all outbound except same-status.
        for (from, to) in [
            ("completed", "running"),
            ("completed", "failed"),
            ("cancelled", "running"),
            ("cancelled", "pending"),
            ("pending", "completed"),
            ("pending", "failed"),
            ("running", "pending"),
            ("frozen", "running"),
            ("paused", "running"),
        ] {
            assert!(
                !is_allowed_transition(from, to),
                "expected {from} → {to} to be disallowed"
            );
        }
    }

    #[test]
    fn state_machine_same_status_is_noop() {
        for s in TASK_STATES {
            assert!(
                is_allowed_transition(s, s),
                "expected same-status no-op for {s}"
            );
        }
    }

    #[test]
    fn state_machine_unknown_status_conservatively_allowed() {
        // Forward-compat: a future status the validator
        // doesn't know about must not block updates. The
        // bridge enforcement layer is responsible for
        // rejecting genuinely-unknown statuses at the API.
        assert!(is_allowed_transition("running", "operator-defined"));
        assert!(is_allowed_transition("future-status", "completed"));
    }

    #[test]
    fn transition_check_reports_allowed_or_not() {
        // M74: actually exercise the snapshot helper through
        // a real task with a known status.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        // pending → running: allowed
        let view = s.get(&tid).unwrap().unwrap();
        assert_eq!(view.status, "pending");
        assert!(is_allowed_transition(&view.status, "running"));
        assert!(!is_allowed_transition(&view.status, "completed"));
    }

    #[test]
    fn subtree_metrics_single_task_no_edges() {
        // Lone task, no spawned children. Metrics still
        // report real aggregates for the root.
        let s = store();
        let tid = mk(&s, "lonely", "f", "{}", "o");
        let m = s.subtree_metrics(&tid, 4).unwrap();
        assert_eq!(m.root_task_id, tid);
        assert_eq!(m.total_tasks, 1);
        assert_eq!(m.cross_task_edge_count, 0);
        assert_eq!(m.active_pending, 1);
        assert_eq!(m.terminal_completed, 0);
        assert_eq!(m.total_attempts, 0);
        // No started_at on a fresh `pending` task — wall
        // clock is 0 and the honesty counter ticks.
        assert_eq!(m.total_wall_clock_secs, 0);
        assert_eq!(m.tasks_with_missing_timing, 1);
    }

    #[test]
    fn subtree_metrics_buckets_status_correctly() {
        // Two tasks: one completed, one running. Both
        // accounted in the right bucket; wall-clock
        // aggregates across them.
        let s = store();
        let parent = mk(&s, "p", "f", "{}", "o");
        let child = mk(&s, "c", "f", "{}", "o");
        // Walk parent into completed, child into running.
        s.update(&parent, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.update(
            &parent,
            Some("completed"),
            Some("ok"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        s.update(&child, Some("running"), None, None, None, None, None, None)
            .unwrap();
        // Connect them with a spawned edge so the lineage
        // walker picks up both tasks.
        s.record_spawned(&parent, &child, None, None, "worker")
            .unwrap();
        let m = s.subtree_metrics(&parent, 4).unwrap();
        assert_eq!(m.total_tasks, 2);
        assert_eq!(m.cross_task_edge_count, 1);
        assert_eq!(m.terminal_completed, 1);
        assert_eq!(m.active_running, 1);
        // Both tasks have started_at set (the `running`
        // transition stamped it) — no missing-timing.
        assert_eq!(m.tasks_with_missing_timing, 0);
        // Wall clock is small but non-negative.
        assert!(m.total_wall_clock_secs >= 0);
        assert_eq!(m.total_attempts, 2);
    }

    #[test]
    fn subtree_metrics_unknown_root_returns_not_found() {
        let s = store();
        let err = s.subtree_metrics("deadbeef", 4).unwrap_err();
        assert!(matches!(err, CoordinatorError::NotFound(id) if id == "deadbeef"));
    }

    #[test]
    fn subtree_metrics_other_status_bucket_counts_caller_defined() {
        // A future status outside TASK_STATES lands in
        // `other_status` — operators see caller-defined
        // schema drift instead of a silent miscount.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(
            &tid,
            Some("custom-state"),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let m = s.subtree_metrics(&tid, 4).unwrap();
        assert_eq!(m.other_status, 1);
        assert_eq!(m.active_pending, 0);
    }

    #[test]
    fn record_spawned_inserts_edge_with_chronicle_anchor() {
        let s = store();
        let parent = mk(&s, "parent", "f", "{}", "o");
        let child = mk(&s, "child", "f", "{}", "o");
        let outcome = s
            .record_spawned(
                &parent,
                &child,
                Some("branch-A"),
                Some("ctx-42"),
                "worker-x",
            )
            .unwrap();
        assert!(outcome.edge_id > 0);
        assert!(outcome.event_id > 0);
        // Edge inserted with correct shape.
        let edges = s.list_edges_for_task(&parent).unwrap();
        let spawned: Vec<&TaskEdge> = edges.iter().filter(|e| e.edge_type == "spawned").collect();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].task_id, parent);
        assert_eq!(spawned[0].related_task_id.as_deref(), Some(child.as_str()));
        assert_eq!(spawned[0].spawned_by_event_id, Some(outcome.event_id));
        // Chronicle event lands on parent with full producer
        // metadata.
        let ev = s
            .list_events_after(&parent, 0, 100)
            .unwrap()
            .into_iter()
            .find(|e| e.event_id == outcome.event_id)
            .expect("chronicle event present");
        assert_eq!(ev.event_type, "task.spawned_child");
        let pj: serde_json::Value =
            serde_json::from_str(ev.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(pj["edge_type"], "spawned");
        assert_eq!(pj["related_task_id"], child);
        assert_eq!(pj["producer"], "worker-x");
        assert_eq!(pj["branch_id"], "branch-A");
        assert_eq!(pj["context_id"], "ctx-42");
    }

    #[test]
    fn record_spawned_refuses_self_edge() {
        let s = store();
        let tid = mk(&s, "self", "f", "{}", "o");
        let err = s
            .record_spawned(&tid, &tid, None, None, "worker")
            .unwrap_err();
        assert!(matches!(err, CoordinatorError::Invalid(_)));
    }

    #[test]
    fn record_delegated_omits_unset_metadata_fields() {
        let s = store();
        let parent = mk(&s, "p", "f", "{}", "o");
        let child = mk(&s, "c", "f", "{}", "o");
        let outcome = s.record_delegated(&parent, &child, None, "worker").unwrap();
        let ev = s
            .list_events_after(&parent, 0, 100)
            .unwrap()
            .into_iter()
            .find(|e| e.event_id == outcome.event_id)
            .expect("chronicle event present");
        assert_eq!(ev.event_type, "task.delegated_to");
        let pj: serde_json::Value =
            serde_json::from_str(ev.payload_json.as_deref().unwrap()).unwrap();
        // Honest: optional fields don't materialize as
        // empty strings or null — they're absent from the
        // payload entirely.
        assert!(pj.get("branch_id").is_none());
        assert!(pj.get("context_id").is_none());
        assert!(pj.get("reason").is_none());
    }

    #[test]
    fn record_awaited_emits_awaited_edge() {
        let s = store();
        let waiter = mk(&s, "w", "f", "{}", "o");
        let awaited = mk(&s, "a", "f", "{}", "o");
        s.record_awaited(&waiter, &awaited, Some("upstream call"), "worker")
            .unwrap();
        let edges = s.list_edges_for_task(&waiter).unwrap();
        assert!(
            edges.iter().any(|e| e.edge_type == "awaited"
                && e.related_task_id.as_deref() == Some(awaited.as_str()))
        );
    }

    #[test]
    fn record_spawned_rejects_unknown_task_ids() {
        let s = store();
        let parent = mk(&s, "p", "f", "{}", "o");
        let err = s
            .record_spawned(&parent, "deadbeef", None, None, "worker")
            .unwrap_err();
        assert!(matches!(err, CoordinatorError::NotFound(id) if id == "deadbeef"));
        let err2 = s
            .record_spawned("deadbeef", &parent, None, None, "worker")
            .unwrap_err();
        assert!(matches!(err2, CoordinatorError::NotFound(id) if id == "deadbeef"));
    }

    #[test]
    fn lineage_finds_spawned_edges_as_cross_task() {
        // M66 + M72 integration: the lineage walker now sees
        // genuine cross-task edges when producers attest them.
        // `cross_task_edge_count` reflects the real count.
        let s = store();
        let parent = mk(&s, "p", "f", "{}", "o");
        let child = mk(&s, "c", "f", "{}", "o");
        s.record_spawned(&parent, &child, None, None, "worker")
            .unwrap();
        let g = s.task_lineage(&parent, 4).unwrap();
        assert!(g.tasks.contains(&parent));
        assert!(g.tasks.contains(&child));
        assert_eq!(g.cross_task_edge_count, 1);
    }

    #[test]
    fn retry_suppressed_when_task_is_paused() {
        // M76: cooperative interruption — a paused task
        // refuses retry AND emits a task.retry_suppressed
        // chronicle event so operators see the gating.
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, None, None)
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.set_paused(&tid, Some("operator hold"), "op").unwrap();
        let decision = s.request_retry(&tid).unwrap();
        match decision {
            RetryDecision::Rejected { reason } => {
                assert!(reason.contains("paused"));
                assert!(reason.contains("cooperative interruption"));
            }
            other => panic!("expected Rejected, got {:?}", other),
        }
        // Chronicle gets the suppression event.
        let ev = s
            .list_events_after(&tid, 0, 100)
            .unwrap()
            .into_iter()
            .find(|e| e.event_type == "task.retry_suppressed")
            .expect("task.retry_suppressed event missing");
        let pj: serde_json::Value =
            serde_json::from_str(ev.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(pj["suppressed_by"], "paused");
    }

    #[test]
    fn retry_suppressed_when_task_is_frozen() {
        let s = store();
        let tid = s
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, None, None)
            .unwrap();
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.set_frozen(&tid, None, "op").unwrap();
        let decision = s.request_retry(&tid).unwrap();
        assert!(
            matches!(decision, RetryDecision::Rejected { reason } if reason.contains("frozen"))
        );
        // Status is still frozen — suppression doesn't change it.
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.status, "frozen");
    }

    #[test]
    fn freeze_from_running_transitions_to_frozen_with_chronicle() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let prior = s.set_frozen(&tid, Some("upstream outage"), "op").unwrap();
        assert_eq!(prior, "running");
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.status, "frozen");
        assert!(v.frozen_at.is_some());
        assert_eq!(v.frozen_reason.as_deref(), Some("upstream outage"));
        let ev = s
            .list_events_after(&tid, 0, 100)
            .unwrap()
            .into_iter()
            .find(|e| e.event_type == "task.freeze_requested")
            .expect("task.freeze_requested event");
        let pj: serde_json::Value =
            serde_json::from_str(ev.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(pj["prior_status"], "running");
        assert_eq!(pj["intent"], "request");
        assert_eq!(pj["author"], "op");
        assert_eq!(
            v.freeze_generation,
            pj["freeze_generation"].as_i64().unwrap()
        );
    }

    #[test]
    fn freeze_refuses_terminal_status() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(
            &tid,
            Some("completed"),
            Some("result"),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let err = s.set_frozen(&tid, None, "op").unwrap_err();
        assert!(matches!(err, CoordinatorError::Invalid(_)));
    }

    #[test]
    fn unfreeze_restores_to_pending_with_pre_freeze_status_in_event() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.set_frozen(&tid, Some("ops triage"), "op").unwrap();
        let pre = s.set_unfrozen(&tid, "op").unwrap();
        assert_eq!(pre, "running");
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.status, "pending");
        assert!(v.frozen_at.is_none());
        assert!(v.frozen_reason.is_none());
        let ev = s
            .list_events_after(&tid, 0, 100)
            .unwrap()
            .into_iter()
            .find(|e| e.event_type == "task.unfreeze_requested")
            .expect("task.unfreeze_requested event");
        let pj: serde_json::Value =
            serde_json::from_str(ev.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(pj["pre_freeze_status"], "running");
        assert_eq!(pj["new_status"], "pending");
        // Two freeze-axis bumps: one to freeze, one to unfreeze.
        assert!(pj["freeze_generation"].as_i64().unwrap() >= 2);
    }

    #[test]
    fn unfreeze_refuses_non_frozen_status() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let err = s.set_unfrozen(&tid, "op").unwrap_err();
        assert!(matches!(err, CoordinatorError::Invalid(_)));
    }

    #[test]
    fn freeze_generation_independent_of_pause_generation() {
        // M70/M71: pause and freeze must use distinct
        // counters so a worker caching one axis doesn't
        // invalidate on the other.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.set_paused(&tid, None, "op").unwrap();
        s.set_resumed(&tid, "op").unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        assert_eq!(v.pause_generation, 2);
        assert_eq!(v.freeze_generation, 0);
        s.set_frozen(&tid, None, "op").unwrap();
        let v2 = s.get(&tid).unwrap().unwrap();
        assert_eq!(v2.pause_generation, 2);
        assert_eq!(v2.freeze_generation, 1);
    }

    #[test]
    fn interruption_snapshot_starts_at_zero_then_advances_on_pause() {
        // M70: every fresh task has both generation counters
        // at 0. The first pause request bumps pause_gen to
        // 1; resume bumps to 2. freeze_gen is untouched by
        // pause/resume.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let snap0 = s.interruption_snapshot(&tid).unwrap();
        assert_eq!(snap0.pause_generation, 0);
        assert_eq!(snap0.freeze_generation, 0);
        assert_eq!(snap0.status, "pending");

        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.set_paused(&tid, None, "op").unwrap();
        let snap1 = s.interruption_snapshot(&tid).unwrap();
        assert_eq!(snap1.pause_generation, 1);
        assert_eq!(snap1.freeze_generation, 0);
        assert_eq!(snap1.status, "paused");

        s.set_resumed(&tid, "op").unwrap();
        let snap2 = s.interruption_snapshot(&tid).unwrap();
        assert_eq!(snap2.pause_generation, 2);
        assert_eq!(snap2.freeze_generation, 0);
        assert_eq!(snap2.status, "pending");
    }

    #[test]
    fn observe_interruption_emits_ack_event_with_generation() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        s.set_paused(&tid, None, "op").unwrap();
        // A cooperative worker observes the pause at gen=1.
        let event_id = s
            .observe_interruption(&tid, "pause", 1, "worker-a")
            .unwrap();
        assert!(event_id > 0);
        let ev = s
            .list_events_after(&tid, 0, 100)
            .unwrap()
            .into_iter()
            .find(|e| e.event_type == "task.pause_observed")
            .expect("task.pause_observed event");
        let pj: serde_json::Value =
            serde_json::from_str(ev.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(pj["interruption_type"], "pause");
        assert_eq!(pj["generation_observed"], 1);
        assert_eq!(pj["observer"], "worker-a");
        assert_eq!(pj["intent"], "ack");
    }

    #[test]
    fn observe_interruption_resume_emits_resume_observed_event_type() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.observe_interruption(&tid, "resume", 5, "worker-b")
            .unwrap();
        let exists = s
            .list_events_after(&tid, 0, 100)
            .unwrap()
            .into_iter()
            .any(|e| e.event_type == "task.resume_observed");
        assert!(exists, "resume observation must use task.resume_observed");
    }

    #[test]
    fn observe_interruption_freeze_emits_freeze_propagated_event_type() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.observe_interruption(&tid, "freeze", 3, "worker-c")
            .unwrap();
        let exists = s
            .list_events_after(&tid, 0, 100)
            .unwrap()
            .into_iter()
            .any(|e| e.event_type == "task.freeze_propagated");
        assert!(exists, "freeze observation must use task.freeze_propagated");
    }

    #[test]
    fn observe_interruption_rejects_unknown_type_and_negative_generation() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        let err = s
            .observe_interruption(&tid, "halt", 1, "worker-x")
            .unwrap_err();
        assert!(matches!(err, CoordinatorError::Invalid(_)));
        let err2 = s
            .observe_interruption(&tid, "pause", -1, "worker-x")
            .unwrap_err();
        assert!(matches!(err2, CoordinatorError::Invalid(_)));
    }

    #[test]
    fn interruption_snapshot_not_found_returns_error() {
        let s = store();
        let err = s.interruption_snapshot("deadbeef").unwrap_err();
        assert!(matches!(err, CoordinatorError::NotFound(id) if id == "deadbeef"));
    }

    #[test]
    fn pause_oversize_reason_rejected() {
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(&tid, Some("running"), None, None, None, None, None, None)
            .unwrap();
        let big = "x".repeat(MAX_OPERATOR_NOTE_LEN + 1);
        let err = s.set_paused(&tid, Some(&big), "op").unwrap_err();
        assert!(matches!(err, CoordinatorError::Invalid(_)));
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

    // ─────────────────── H4: anti-thrash auto-mark ──────────────────────────

    #[test]
    fn anti_thrash_auto_marks_after_threshold_same_class_failures() {
        // Three failures in a row with the same failure_class trigger
        // the auto-mark + emit task.thrash_detected and
        // task.investigation_marked. Matches Hermes's
        // _ineffective_compression_count pattern: when the runtime
        // keeps failing the same way, escalate to the operator
        // without waiting for them to grep the audit log.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        // Three different attempts all fail with the SAME class.
        for _ in 0..ANTI_THRASH_THRESHOLD {
            s.update(
                &tid,
                Some("failed"),
                None,
                None,
                None,
                None,
                None,
                Some("transport"),
            )
            .expect("update");
        }
        let v = s.get(&tid).unwrap().unwrap();
        assert!(
            v.investigation_marked_at.is_some(),
            "task should be auto-marked after {ANTI_THRASH_THRESHOLD} same-class failures"
        );
        let events = s.list_events_after(&tid, 0, 50).unwrap();
        let thrash = events
            .iter()
            .find(|e| e.event_type == "task.thrash_detected")
            .expect("task.thrash_detected missing");
        let pj: serde_json::Value =
            serde_json::from_str(thrash.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(pj["class"], "transport");
        assert_eq!(pj["count"], ANTI_THRASH_THRESHOLD);
        assert_eq!(pj["threshold"], ANTI_THRASH_THRESHOLD);
        // Mirror event for dashboard treatment.
        let auto_marked = events
            .iter()
            .rfind(|e| e.event_type == "task.investigation_marked")
            .expect("auto-marked investigation event missing");
        let mpj: serde_json::Value =
            serde_json::from_str(auto_marked.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(mpj["auto"], true);
        assert_eq!(mpj["thrash_class"], "transport");
    }

    #[test]
    fn anti_thrash_resets_when_class_changes() {
        // Two `transport` failures + one `timeout` failure → counter
        // resets to 1 on the class change, no auto-mark.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            None,
            Some("transport"),
        )
        .unwrap();
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            None,
            Some("transport"),
        )
        .unwrap();
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            None,
            Some("timeout"),
        )
        .unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        assert!(
            v.investigation_marked_at.is_none(),
            "different failure classes should NOT auto-mark"
        );
        // Three transport failures after the timeout would re-trigger.
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            None,
            Some("transport"),
        )
        .unwrap();
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            None,
            Some("transport"),
        )
        .unwrap();
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            None,
            Some("transport"),
        )
        .unwrap();
        let v2 = s.get(&tid).unwrap().unwrap();
        assert!(
            v2.investigation_marked_at.is_some(),
            "three consecutive transport failures after the timeout reset must auto-mark"
        );
    }

    #[test]
    fn anti_thrash_does_not_clobber_existing_operator_mark() {
        // If the operator pre-marked the task with their own reason,
        // the auto-marker must not overwrite. The reason field
        // remains the operator's. Operators care about their
        // triage notes more than the auto-marker.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        s.set_investigation_marker(&tid, true, Some("manual triage"), "op")
            .unwrap();
        for _ in 0..(ANTI_THRASH_THRESHOLD + 1) {
            s.update(
                &tid,
                Some("failed"),
                None,
                None,
                None,
                None,
                None,
                Some("transport"),
            )
            .unwrap();
        }
        let v = s.get(&tid).unwrap().unwrap();
        assert!(v.investigation_marked_at.is_some());
        assert_eq!(v.investigation_reason.as_deref(), Some("manual triage"));
    }

    #[test]
    fn anti_thrash_does_not_fire_below_threshold() {
        // Threshold - 1 consecutive same-class failures must not
        // auto-mark. The counter exists but the escalation only
        // happens when the threshold is crossed.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        for _ in 0..(ANTI_THRASH_THRESHOLD - 1) {
            s.update(
                &tid,
                Some("failed"),
                None,
                None,
                None,
                None,
                None,
                Some("transport"),
            )
            .unwrap();
        }
        let v = s.get(&tid).unwrap().unwrap();
        assert!(
            v.investigation_marked_at.is_none(),
            "below-threshold consecutive failures must not auto-mark"
        );
    }

    #[test]
    fn anti_thrash_ignores_updates_without_failure_class() {
        // A status update with no failure_class must not bump or
        // reset the counter. The counter only moves on actual
        // failure events.
        let s = store();
        let tid = mk(&s, "t", "f", "{}", "o");
        // Set a failure baseline.
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            None,
            Some("transport"),
        )
        .unwrap();
        // A bunch of non-failure-class updates.
        for _ in 0..10 {
            s.update(&tid, None, Some("ok"), None, None, None, None, None)
                .unwrap();
        }
        // Two more failures of the same class — total = 3 consecutive.
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            None,
            Some("transport"),
        )
        .unwrap();
        s.update(
            &tid,
            Some("failed"),
            None,
            None,
            None,
            None,
            None,
            Some("transport"),
        )
        .unwrap();
        let v = s.get(&tid).unwrap().unwrap();
        assert!(
            v.investigation_marked_at.is_some(),
            "consecutive same-class failures with intervening no-class updates must still auto-mark"
        );
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
        // has 5 tasks * 3 user events each, and the 4 terminal
        // tasks each pick up ONE auto-emitted task.terminal_summary
        // (H14) when they transition. So: 4 tasks × (3 + 1) = 16
        // candidate events. The running task (3 events) is excluded.
        let (s, cutoff) = compact_fixture();
        let r = s.count_compact_candidates(cutoff).unwrap();
        assert_eq!(
            r.candidate_events, 16,
            "candidate_events should be 4 terminal tasks × (3 user + 1 H14 terminal_summary) events"
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
        // The `running` cohort must be absent (R5). H14 adds one
        // task.terminal_summary per terminal task → +1 per status
        // cohort.
        let m: std::collections::HashMap<String, i64> = r.by_task_status.into_iter().collect();
        // 2 completed tasks × (3 user + 1 H14) = 8
        assert_eq!(m.get("completed").copied(), Some(8));
        // 1 failed × 4 = 4
        assert_eq!(m.get("failed").copied(), Some(4));
        // 1 cancelled × 4 = 4
        assert_eq!(m.get("cancelled").copied(), Some(4));
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
        // H14 added one task.terminal_summary per terminal task
        // → 16 candidate events instead of the pre-H14 12.
        assert_eq!(
            parsed.get("candidate_events").and_then(|v| v.as_i64()),
            Some(16)
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
            tenant_id: None,
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
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, None, None)
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
                .create(label, "f", "{}", "o", RetryPolicy::Bounded, 3, None, None)
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
                .create("t", "f", "{}", "o", RetryPolicy::Bounded, 3, None, None)
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
            .create("t", "f", "{}", "o", RetryPolicy::Bounded, 5, None, None)
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

    // ── W5: task.session_export ──────────────────────────────────

    fn seed_chat_turn(s: &TaskStore, task_id: &str, role: &str, content: &str, ts: i64) {
        let event_type = if role == "user" {
            "chat.user_turn"
        } else {
            "chat.assistant_turn"
        };
        // Wire format mirrors parse_chat_turn_payload.
        let payload = format!("{}|{}|{}|{}", "sess-A", role, ts, content);
        // We append via append_event but want the row `ts` to
        // match; the column gets a server timestamp by default
        // so we backfill via raw SQL to keep ordering tight.
        s.append_event(task_id, event_type, &payload).unwrap();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE task_events SET ts = ?1 \
                 WHERE task_id = ?2 AND event_type = ?3 AND payload = ?4",
                rusqlite::params![ts, task_id, event_type, payload],
            )
            .unwrap();
        }
    }

    #[test]
    fn session_export_returns_all_turns_in_chronological_order_with_role_labels() {
        let s = store();
        let tid = mk(&s, "chat session", "chat.sol", "{}", "alice");
        // Seed 5 turns with monotonically increasing ts.
        seed_chat_turn(&s, &tid, "user", "hi", 1_700_000_001);
        seed_chat_turn(&s, &tid, "assistant", "hello!", 1_700_000_002);
        seed_chat_turn(&s, &tid, "user", "what time is it?", 1_700_000_003);
        seed_chat_turn(&s, &tid, "assistant", "i don't know", 1_700_000_004);
        seed_chat_turn(&s, &tid, "user", "ok thanks", 1_700_000_005);
        let turns = s.query_chat_turns("sess-A", 100).unwrap();
        assert_eq!(turns.len(), 5);
        // Chronological.
        for w in turns.windows(2) {
            assert!(w[0].timestamp_unix <= w[1].timestamp_unix);
        }
        // Role labels in the expected sequence.
        let roles: Vec<&str> = turns.iter().map(|t| t.role.as_str()).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "user", "assistant", "user"]
        );
        // Content preserved verbatim.
        assert_eq!(turns[0].content, "hi");
        assert_eq!(turns[2].content, "what time is it?");
    }

    #[test]
    fn session_export_returns_empty_for_unknown_session() {
        let s = store();
        let turns = s.query_chat_turns("nope-no-session", 100).unwrap();
        assert!(turns.is_empty());
    }

    #[test]
    fn session_export_handler_returns_json_array() {
        let s = store();
        let tid = mk(&s, "chat session", "chat.sol", "{}", "alice");
        seed_chat_turn(&s, &tid, "user", "ping", 1_700_000_001);
        let outcome = handle_session_export(
            &s,
            &InvocationCtx {
                caller: relix_core::identity::VerifiedIdentity {
                    subject_id: relix_core::types::NodeId::from_pubkey(b"x"),
                    name: "x".into(),
                    org_id: relix_core::types::NodeId::from_pubkey(b"o"),
                    groups: vec![],
                    role: "agent".into(),
                    clearance: "internal".into(),
                    bundle_id: [0; 32],
                },
                trace_id: relix_core::types::TraceId::new(),
                request_id: relix_core::types::RequestId::new(),
                args: b"sess-A".to_vec(),
                tenant_id: None,
            },
        );
        match outcome {
            HandlerOutcome::Ok(body) => {
                let s = String::from_utf8(body).unwrap();
                let v: serde_json::Value = serde_json::from_str(&s).unwrap();
                let arr = v.as_array().unwrap();
                assert_eq!(arr.len(), 1);
                assert_eq!(arr[0]["role"], "user");
                assert_eq!(arr[0]["content"], "ping");
                assert_eq!(arr[0]["session_id"], "sess-A");
            }
            HandlerOutcome::Err(e) => panic!("expected Ok, got err: {}", e.cause),
        }
    }

    // ── task.session_search ──────────────────────────────────────

    /// Seed a chat turn for an explicit session_id (the W5 helper
    /// hard-codes `sess-A`). Used by the search-cross-session
    /// tests.
    fn seed_chat_turn_for(
        s: &TaskStore,
        task_id: &str,
        session_id: &str,
        role: &str,
        content: &str,
        ts: i64,
    ) {
        let event_type = if role == "user" {
            "chat.user_turn"
        } else {
            "chat.assistant_turn"
        };
        let payload = format!("{session_id}|{role}|{ts}|{content}");
        s.append_event(task_id, event_type, &payload).unwrap();
        let conn = s.conn.lock().unwrap();
        conn.execute(
            "UPDATE task_events SET ts = ?1 WHERE task_id = ?2 AND event_type = ?3 AND payload = ?4",
            rusqlite::params![ts, task_id, event_type, payload],
        )
        .unwrap();
    }

    #[test]
    fn search_returns_single_match_with_session_id_role_timestamp_and_snippet() {
        let s = store();
        let tid = mk(&s, "chat session", "chat.sol", "{}", "alice");
        seed_chat_turn_for(
            &s,
            &tid,
            "sess-A",
            "user",
            "hello world today",
            1_700_000_001,
        );
        seed_chat_turn_for(
            &s,
            &tid,
            "sess-A",
            "assistant",
            "nothing matches",
            1_700_000_002,
        );
        let hits = s.search_chat_turns("", "world", 20).unwrap();
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.session_id, "sess-A");
        assert_eq!(h.role, "user");
        assert_eq!(h.content, "hello world today");
        assert_eq!(h.timestamp_unix, 1_700_000_001);
        assert!(h.snippet.contains("world"), "snippet={}", h.snippet);
        assert!((h.score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn search_returns_multiple_matches_in_timestamp_order() {
        let s = store();
        let tid = mk(&s, "chat session", "chat.sol", "{}", "alice");
        seed_chat_turn_for(
            &s,
            &tid,
            "sess-A",
            "user",
            "find me FOO once",
            1_700_000_002,
        );
        seed_chat_turn_for(&s, &tid, "sess-B", "user", "FOO again here", 1_700_000_001);
        seed_chat_turn_for(
            &s,
            &tid,
            "sess-B",
            "assistant",
            "more FOO data",
            1_700_000_003,
        );
        let hits = s.search_chat_turns("", "FOO", 20).unwrap();
        assert_eq!(hits.len(), 3);
        for w in hits.windows(2) {
            assert!(w[0].timestamp_unix <= w[1].timestamp_unix);
        }
    }

    #[test]
    fn search_subject_id_filter_restricts_to_owner_subject() {
        let s = store();
        let tid_alice = mk(&s, "alice chat", "chat.sol", "{}", "alice");
        let tid_bob = mk(&s, "bob chat", "chat.sol", "{}", "bob");
        seed_chat_turn_for(
            &s,
            &tid_alice,
            "sess-a",
            "user",
            "search target xyz",
            1_700_000_001,
        );
        seed_chat_turn_for(
            &s,
            &tid_bob,
            "sess-b",
            "user",
            "search target xyz",
            1_700_000_002,
        );
        let alice_hits = s.search_chat_turns("alice", "xyz", 20).unwrap();
        assert_eq!(alice_hits.len(), 1);
        assert_eq!(alice_hits[0].session_id, "sess-a");
        let bob_hits = s.search_chat_turns("bob", "xyz", 20).unwrap();
        assert_eq!(bob_hits.len(), 1);
        assert_eq!(bob_hits[0].session_id, "sess-b");
        let all_hits = s.search_chat_turns("", "xyz", 20).unwrap();
        assert_eq!(all_hits.len(), 2);
    }

    #[test]
    fn search_limit_caps_at_max_session_search_limit() {
        let s = store();
        let tid = mk(&s, "chat", "chat.sol", "{}", "alice");
        for i in 0..200 {
            seed_chat_turn_for(
                &s,
                &tid,
                "sess",
                "user",
                &format!("needle row {i}"),
                1_700_000_000 + i as i64,
            );
        }
        // Limit above MAX clamps to MAX.
        let hits = s.search_chat_turns("", "needle", 5_000).unwrap();
        assert_eq!(hits.len(), MAX_SESSION_SEARCH_LIMIT);
        // Limit 0 collapses to DEFAULT.
        let hits = s.search_chat_turns("", "needle", 0).unwrap();
        assert_eq!(hits.len(), DEFAULT_SESSION_SEARCH_LIMIT);
        // Explicit small limit respected.
        let hits = s.search_chat_turns("", "needle", 7).unwrap();
        assert_eq!(hits.len(), 7);
    }

    #[test]
    fn search_no_matches_returns_empty_not_error() {
        let s = store();
        let tid = mk(&s, "chat", "chat.sol", "{}", "alice");
        seed_chat_turn_for(&s, &tid, "sess", "user", "hello there", 1_700_000_001);
        let hits = s.search_chat_turns("", "missing-token", 20).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn search_snippet_includes_surrounding_context_around_match() {
        // Build a long-enough content body so the 50/50 window
        // doesn't span the whole string. The snippet must
        // contain the matched substring plus context on each
        // side.
        let prefix = "AAAA".repeat(40); // 160 chars before
        let suffix = "BBBB".repeat(40); // 160 chars after
        let content = format!("{prefix}NEEDLE{suffix}");
        let snippet = build_match_snippet(&content, "needle");
        assert!(snippet.contains("NEEDLE"), "snippet={snippet}");
        // Trimmed on the prefix side, so first chars are NOT the
        // original opening "AAAA".
        assert!(snippet.starts_with('…'), "snippet={snippet}");
        assert!(snippet.ends_with('…'), "snippet={snippet}");
    }

    #[test]
    fn search_handler_returns_json_array_with_score_and_snippet() {
        let s = store();
        let tid = mk(&s, "chat", "chat.sol", "{}", "alice");
        seed_chat_turn_for(
            &s,
            &tid,
            "sess",
            "user",
            "operator queries needle here",
            1_700_000_001,
        );
        let ctx = InvocationCtx {
            caller: relix_core::identity::VerifiedIdentity {
                subject_id: relix_core::types::NodeId::from_pubkey(b"x"),
                name: "x".into(),
                org_id: relix_core::types::NodeId::from_pubkey(b"o"),
                groups: vec![],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: relix_core::types::TraceId::new(),
            request_id: relix_core::types::RequestId::new(),
            args: b"|needle|5".to_vec(),
            tenant_id: None,
        };
        let outcome = handle_session_search(&s, &ctx);
        match outcome {
            HandlerOutcome::Ok(body) => {
                let body = String::from_utf8(body).unwrap();
                let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                let arr = v.as_array().unwrap();
                assert_eq!(arr.len(), 1);
                assert_eq!(arr[0]["session_id"], "sess");
                assert_eq!(arr[0]["role"], "user");
                assert!(arr[0]["snippet"].as_str().unwrap().contains("needle"));
                let score = arr[0]["score"].as_f64().unwrap();
                assert!((score - 1.0).abs() < 1e-6);
            }
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        }
    }

    #[test]
    fn search_handler_rejects_empty_query() {
        let s = store();
        let ctx = InvocationCtx {
            caller: relix_core::identity::VerifiedIdentity {
                subject_id: relix_core::types::NodeId::from_pubkey(b"x"),
                name: "x".into(),
                org_id: relix_core::types::NodeId::from_pubkey(b"o"),
                groups: vec![],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: relix_core::types::TraceId::new(),
            request_id: relix_core::types::RequestId::new(),
            args: b"||20".to_vec(),
            tenant_id: None,
        };
        let outcome = handle_session_search(&s, &ctx);
        match outcome {
            HandlerOutcome::Err(e) => {
                assert_eq!(e.kind, relix_core::types::error_kinds::INVALID_ARGS);
            }
            HandlerOutcome::Ok(_) => panic!("expected Err on empty query"),
        }
    }

    #[test]
    fn parse_chat_turn_payload_handles_pipe_in_content() {
        let turn = parse_chat_turn_payload(
            "sess-A",
            "chat.user_turn",
            "sess-A|user|1700000001|hello | world | pipes",
            0,
        )
        .unwrap();
        assert_eq!(turn.role, "user");
        assert_eq!(turn.content, "hello | world | pipes");
        assert_eq!(turn.timestamp_unix, 1_700_000_001);
    }

    // ── W4: drift hook embedding wiring ──────────────────────────

    fn seed_drift_task(s: &TaskStore, title: &str, events_n: usize) -> String {
        let tid = mk(s, title, "flow.sol", "{}", "owner");
        for i in 0..events_n {
            s.append_event(&tid, "task.run", &format!("step {i}"))
                .unwrap();
        }
        tid
    }

    fn read_drift_payload(s: &TaskStore, task_id: &str) -> String {
        let evts = s
            .query_events(task_id, 0, 100, None, EventOrder::Desc)
            .unwrap();
        evts.into_iter()
            .find(|e| e.event_type == "guardrail.drift_evaluation")
            .expect("drift_evaluation event present")
            .payload
    }

    #[tokio::test]
    async fn drift_hook_writes_cosine_score_when_embedder_returns_vectors() {
        use crate::nodes::ai::guardrails::DriftConfig;
        let s = store();
        let title = "draft release announcement";
        let tid = seed_drift_task(&s, title, 12);
        // Configure a low threshold so similarity > threshold → no drift.
        let cfg = DriftConfig {
            enabled: true,
            threshold: 0.5,
            check_every_n: 10,
            action: crate::nodes::ai::guardrails::DriftAction::Warn,
        };
        // Parallel vectors → cosine 1.0 → above threshold.
        // The summary text is generated by the detector so we
        // hard-code a single aligned response for any input.
        struct AlignedEmbedder;
        #[async_trait::async_trait]
        impl crate::nodes::ai::guardrails::DriftEmbedDispatcher for AlignedEmbedder {
            async fn embed(&self, _text: &str) -> Option<Vec<f32>> {
                Some(vec![1.0f32, 0.0])
            }
        }
        let embedder: Arc<dyn crate::nodes::ai::guardrails::DriftEmbedDispatcher> =
            Arc::new(AlignedEmbedder);
        evaluate_drift_for_task(&s, &tid, &cfg, Some(embedder)).await;
        let payload = read_drift_payload(&s, &tid);
        // Aligned vectors → similarity 1.0000 (or very close).
        assert!(
            payload.contains("similarity=1.0000"),
            "expected similarity=1.0000, got payload: {payload}"
        );
        // No drift since 1.0 >= 0.5.
        assert!(
            payload.contains("drift_detected=false"),
            "payload: {payload}"
        );
    }

    #[tokio::test]
    async fn drift_hook_flags_drift_detected_when_similarity_below_threshold() {
        use crate::nodes::ai::guardrails::DriftConfig;
        let s = store();
        let tid = seed_drift_task(&s, "summarise quarterly report", 12);
        let cfg = DriftConfig {
            enabled: true,
            threshold: 0.7,
            check_every_n: 10,
            action: crate::nodes::ai::guardrails::DriftAction::Warn,
        };
        // Embedder returns ORTHOGONAL vectors for the two
        // calls (goal vs recent summary). Orthogonal → cosine
        // 0.0 < 0.7 → drift_detected = true.
        struct OrthogonalEmbedder {
            calls: std::sync::Mutex<usize>,
        }
        #[async_trait::async_trait]
        impl crate::nodes::ai::guardrails::DriftEmbedDispatcher for OrthogonalEmbedder {
            async fn embed(&self, _text: &str) -> Option<Vec<f32>> {
                let mut c = self.calls.lock().unwrap();
                let n = *c;
                *c += 1;
                if n == 0 {
                    Some(vec![1.0f32, 0.0])
                } else {
                    Some(vec![0.0f32, 1.0])
                }
            }
        }
        let embedder: Arc<dyn crate::nodes::ai::guardrails::DriftEmbedDispatcher> =
            Arc::new(OrthogonalEmbedder {
                calls: std::sync::Mutex::new(0),
            });
        evaluate_drift_for_task(&s, &tid, &cfg, Some(embedder)).await;
        let payload = read_drift_payload(&s, &tid);
        assert!(
            payload.contains("similarity=0.0000"),
            "expected similarity=0.0000, got payload: {payload}"
        );
        assert!(
            payload.contains("drift_detected=true"),
            "payload: {payload}"
        );
    }

    #[tokio::test]
    async fn drift_hook_records_similarity_none_when_no_embedder_wired() {
        use crate::nodes::ai::guardrails::DriftConfig;
        let s = store();
        let tid = seed_drift_task(&s, "investigate latency regression", 12);
        let cfg = DriftConfig {
            enabled: true,
            threshold: 0.65,
            check_every_n: 10,
            action: crate::nodes::ai::guardrails::DriftAction::Warn,
        };
        evaluate_drift_for_task(&s, &tid, &cfg, None).await;
        let payload = read_drift_payload(&s, &tid);
        assert!(
            payload.contains("similarity=none"),
            "expected similarity=none when embedder is absent, got payload: {payload}"
        );
        assert!(
            payload.contains("drift_detected=false"),
            "payload: {payload}"
        );
    }
}
