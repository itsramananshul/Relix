//! `tool.terminal.run` — sandboxed shell command execution
//! (Capability Wave CW1).
//!
//! ## Security model — fail closed
//!
//! Terminal execution is the highest-blast-radius capability
//! Relix exposes. The fail-closed posture is layered:
//!
//! 1. **Opt-in registration.** No `[tool.terminal]` section
//!    in the controller TOML → the capability is not
//!    registered at all.
//! 2. **Allowlist enforcement.** The config carries a
//!    `allowed_commands` set of EXACT binary names. A
//!    requested command not in the set is rejected with
//!    `INVALID_ARGS` before any process spawn. The set is
//!    intentionally just program names (no paths, no
//!    wildcards) so the operator's policy is auditable.
//! 3. **No shell.** Commands run via `tokio::process::Command`
//!    with `args` as a separate vector — no shell interpolation,
//!    no `sh -c`, no string concatenation. Operators who need
//!    a shell pipeline must build it inside their flow code
//!    and call the capability for each step.
//! 4. **Path-traversal-free command lookup.** The command
//!    name must contain no `/` or `\` separator — the OS
//!    PATH does the resolution. Operators wanting to pin a
//!    specific binary use a more restrictive `PATH` env or
//!    a wrapper script.
//! 5. **Hard timeout.** Every spawn is wrapped in
//!    `tokio::time::timeout`. On expiry the child is killed
//!    and the response carries `timed_out: true` with whatever
//!    output was captured up to that point.
//! 6. **Output caps.** stdout + stderr each capped at
//!    `MAX_OUTPUT_BYTES`. Overflow is truncated with
//!    `truncated_stdout` / `truncated_stderr` flags.
//! 7. **No env inheritance by default.** Spawned process
//!    sees an empty env unless `inherit_env: true` in
//!    config. Operators must opt in deliberately.
//!
//! ## Cancellation
//!
//! As of PH-TERM-CANCEL the run path races `child.wait()` against
//! an `Arc<tokio::sync::Notify>` held on the session record. The
//! companion capability `tool.terminal.cancel|<session_id>`
//! triggers the notify; the run task then kills the child and
//! returns a response with `cancelled: true`. Hard timeout
//! remains the safety floor — cancel is cooperative on top of it,
//! not a replacement.
//!
//! ## Streaming output (PH-TERM-STREAM1)
//!
//! The stdout/stderr buffers live on the session record while
//! the run is in flight. Operators poll
//! `tool.terminal.tail|<session_id>|<stream>|<offset>` to pull
//! new bytes by cursor — the response carries `next_offset`,
//! the chunk (lossy-UTF-8), and a `truncated` flag (64 KiB
//! per-call cap). Once the session is removed from the registry
//! the operator should pull the final output from the
//! `tool.terminal.run` response.
//!
//! The bounded buffer cap (`MAX_OUTPUT_BYTES` = 1 MiB per
//! stream) is unchanged — once the buffer fills, the drainer
//! stops reading, the OS pipe buffer fills, and the child
//! blocks on write. Streaming is observability, not
//! backpressure relief; a future ring-with-consumer-cursor
//! would relax this.
//!
//! ## Still out of scope (alpha)
//!
//! - No streaming-with-consumer-drain (tail is read-only; it
//!   does NOT advance the drainer's write head, so a long-
//!   running run producing > 1 MiB still stalls).
//! - No background / detached execution.
//! - No persistent shell sessions.
//! - No interactive stdin.
//!
//! These are explicit future-work items, not silent omissions.
//! The chronicle entry the bridge would write against a calling
//! task records the exit code + duration, which is enough for
//! post-hoc debugging; streaming lands alongside the live
//! firehose consumer that needs it.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use relix_core::capability::{CapabilityDescriptor, CostClass, Idempotency};
use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

/// Per-node terminal subsystem config. Opt-in: when this
/// section is missing the capability is not registered.
#[derive(Clone, Debug, Deserialize)]
pub struct TerminalConfig {
    /// Exact program names the operator has allowed (no
    /// paths, no globs). A spawn request for any other
    /// command returns `INVALID_ARGS`.
    pub allowed_commands: Vec<String>,
    /// Hard ceiling on per-run wall clock, seconds. Default
    /// `DEFAULT_TIMEOUT_SECS`. Requests may set a smaller
    /// per-call timeout but never larger; the smaller of the
    /// two wins.
    #[serde(default = "default_timeout_secs")]
    pub max_timeout_secs: u64,
    /// Whether spawned children inherit the controller's env
    /// vars. Default `false` — fail-closed posture so
    /// secrets in the controller's env don't leak into
    /// arbitrary spawned binaries.
    #[serde(default)]
    pub inherit_env: bool,
    /// Optional working directory for spawned children.
    /// `None` → the controller's cwd. Operators wanting
    /// fs-jail discipline pass a dedicated scratch dir.
    #[serde(default)]
    pub working_dir: Option<std::path::PathBuf>,
}

fn default_timeout_secs() -> u64 {
    30
}

/// Hard cap on stdout/stderr capture per stream. Overflow is
/// truncated + flagged in the response.
const MAX_OUTPUT_BYTES: usize = 1_048_576; // 1 MiB

/// PH-TERM-SESSIONS / PH-TERM-CANCEL / PH-TERM-STREAM1: one live
/// `tool.terminal.run` invocation in flight. Inserted on spawn,
/// removed on completion (success, timeout, cancel, or spawn
/// failure). The run task awaits `cancel_notify.notified()` in a
/// select with wait / timeout; the `tool.terminal.cancel` capability
/// triggers the notify to terminate the child cooperatively. The
/// stdout/stderr buffers are shared with the drainer tasks and the
/// `tool.terminal.tail` poller.
#[derive(Clone, Debug)]
pub struct TerminalSessionRecord {
    pub session_id: String,
    /// OS process id captured immediately after spawn. `None`
    /// when the platform doesn't expose the pid (very rare).
    pub pid: Option<u32>,
    pub command: String,
    /// Args as supplied by the caller. Copied so the registry
    /// survives request scope.
    pub args: Vec<String>,
    /// Unix seconds at spawn time.
    pub started_at: i64,
    /// Hex `subject_id` of the caller.
    pub caller_subject_id: String,
    /// Effective per-call timeout (after clamping against
    /// `max_timeout_secs`).
    pub timeout_secs: u64,
    /// PH-TERM-CANCEL: trigger handle for `tool.terminal.cancel`.
    /// `notify_one()` from the cancel handler stores a permit
    /// even if the run task hasn't yet started awaiting, so the
    /// register-then-await race is closed.
    pub cancel_notify: Arc<tokio::sync::Notify>,
    /// PH-TERM-STREAM1: live stdout buffer shared with the
    /// drainer task and the `tool.terminal.tail` poller. Grows
    /// until `MAX_OUTPUT_BYTES`; never reset.
    pub stdout_buf: Arc<Mutex<Vec<u8>>>,
    /// PH-TERM-STREAM1: live stderr buffer — same shape as
    /// `stdout_buf`.
    pub stderr_buf: Arc<Mutex<Vec<u8>>>,
}

/// PH-TERM-AUDIT: one completed `tool.terminal.run` invocation
/// observation. Pushed onto the bounded audit ring after every
/// terminated run regardless of outcome (normal exit, timeout,
/// cancel, or wait-error). Pure in-memory observability — does
/// NOT replace the dispatch-level audit log, does NOT duplicate
/// chronicle.
#[derive(Clone, Debug)]
pub struct TerminalAuditEntry {
    /// Wall-clock unix seconds at the moment of completion.
    pub ts_secs: i64,
    pub command: String,
    pub args: Vec<String>,
    /// Exit code as reported by the OS. `None` when the child
    /// was killed (timeout / cancel) or wait failed.
    pub exit_code: Option<i32>,
    /// Wall-clock elapsed from spawn to termination, in
    /// milliseconds.
    pub duration_ms: u64,
    pub timed_out: bool,
    /// PH-TERM-CANCEL: true when the run was terminated by
    /// `tool.terminal.cancel` rather than by natural exit or
    /// timeout. `timed_out` and `cancelled` are mutually
    /// exclusive — at most one is set on any given entry.
    pub cancelled: bool,
    /// Hex `subject_id` of the caller.
    pub caller_subject_id: String,
}

/// PH-TERM-AUDIT: bounded ring of [`TerminalAuditEntry`].
#[derive(Debug)]
pub struct TerminalAuditRing {
    entries: Mutex<VecDeque<TerminalAuditEntry>>,
    capacity: usize,
}

impl TerminalAuditRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity: capacity.max(1),
        }
    }

    pub fn push(&self, e: TerminalAuditEntry) {
        let mut g = self
            .entries
            .lock()
            .expect("tool.terminal audit ring poisoned");
        if g.len() == self.capacity {
            g.pop_front();
        }
        g.push_back(e);
    }

    pub fn snapshot_newest_first(&self, max: usize) -> Vec<TerminalAuditEntry> {
        let g = self
            .entries
            .lock()
            .expect("tool.terminal audit ring poisoned");
        g.iter().rev().take(max).cloned().collect()
    }
}

/// Default ring capacity. Bounded so a busy operator can't
/// hold an unbounded history in process memory.
const TERMINAL_AUDIT_RING_DEFAULT: usize = 256;

/// Validated terminal config + the allowlist as a hash set
/// for O(1) lookup.
#[derive(Debug)]
pub struct TerminalBackend {
    cfg: TerminalConfig,
    allowed: BTreeSet<String>,
    /// PH-TERM-SESSIONS: live in-flight runs. Keyed by the
    /// session id allocated at spawn. Mutex-bounded; held only
    /// for short insert/remove/snapshot transactions.
    sessions: Mutex<HashMap<String, TerminalSessionRecord>>,
    /// PH-TERM-AUDIT: bounded ring of completed runs.
    audit: TerminalAuditRing,
}

impl TerminalBackend {
    pub fn new(cfg: TerminalConfig) -> Result<Self, String> {
        if cfg.allowed_commands.is_empty() {
            return Err(
                "tool.terminal: allowed_commands must list at least one binary; \
                 the capability fails closed when no allowlist is provided"
                    .to_string(),
            );
        }
        for cmd in &cfg.allowed_commands {
            if cmd.is_empty() {
                return Err("tool.terminal: allowed_commands contains empty entry".to_string());
            }
            if cmd.contains('/') || cmd.contains('\\') {
                return Err(format!(
                    "tool.terminal: allowed_commands entry `{cmd}` contains a path \
                     separator; only bare program names are accepted"
                ));
            }
        }
        if cfg.max_timeout_secs == 0 {
            return Err(
                "tool.terminal: max_timeout_secs must be > 0 (use a reasonable value, \
                 not zero — the runtime needs a hard ceiling)"
                    .to_string(),
            );
        }
        let allowed: BTreeSet<String> = cfg.allowed_commands.iter().cloned().collect();
        Ok(Self {
            cfg,
            allowed,
            sessions: Mutex::new(HashMap::new()),
            audit: TerminalAuditRing::new(TERMINAL_AUDIT_RING_DEFAULT),
        })
    }

    /// PH-TERM-SESSIONS: snapshot the live session table. Held
    /// in a short mutex critical section; returned records are
    /// cloned so the caller is free to format them outside the
    /// lock.
    pub fn snapshot_sessions(&self) -> Vec<TerminalSessionRecord> {
        let g = self
            .sessions
            .lock()
            .expect("tool.terminal sessions poisoned");
        g.values().cloned().collect()
    }

    /// PH-TERM-AUDIT: snapshot the most recent N completed runs.
    pub fn audit_snapshot(&self, max: usize) -> Vec<TerminalAuditEntry> {
        self.audit.snapshot_newest_first(max)
    }
}

/// Wire-shape request body. Operators submit a JSON object
/// over the dispatch envelope.
#[derive(Debug, Deserialize)]
struct RunRequest {
    /// Bare program name (must be in `allowed_commands`).
    command: String,
    /// Argv tail. NOT subject to shell interpretation —
    /// passed verbatim to the OS spawn.
    #[serde(default)]
    args: Vec<String>,
    /// Optional per-call timeout. Clamped to
    /// `cfg.max_timeout_secs`. `None` → use the config max.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Wire-shape response body.
#[derive(Debug, Serialize)]
struct RunResponse {
    /// Exit status as reported by the OS. `None` when the
    /// process was killed (timeout / cancel — `timed_out` /
    /// `cancelled` disambiguate).
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    duration_ms: u64,
    /// True when the timeout fired and we killed the child
    /// before it exited naturally.
    timed_out: bool,
    /// PH-TERM-CANCEL: true when `tool.terminal.cancel` fired
    /// for this session's id and we killed the child. Mutually
    /// exclusive with `timed_out`.
    cancelled: bool,
    /// True when stdout exceeded `MAX_OUTPUT_BYTES` and was
    /// truncated.
    truncated_stdout: bool,
    truncated_stderr: bool,
    /// The command that ran + the effective timeout that was
    /// applied. Operators see both for post-hoc audit.
    command: String,
    timeout_secs: u64,
}

/// Register the `tool.terminal.*` capabilities on the dispatch
/// bridge. Called from `tool::register` when the `[tool.terminal]`
/// config section is present.
pub fn register(bridge: &mut DispatchBridge, backend: Arc<TerminalBackend>) {
    {
        let b = backend.clone();
        bridge.register(
            "tool.terminal.run",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let b = b.clone();
                async move { handle_run(b, ctx).await }
            })),
        );
    }
    {
        let b = backend.clone();
        bridge.register(
            "tool.terminal.sessions",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let b = b.clone();
                async move { handle_sessions(b, &ctx) }
            })),
        );
    }
    {
        let b = backend.clone();
        bridge.register(
            "tool.terminal.audit_recent",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let b = b.clone();
                async move { handle_audit_recent(b, &ctx) }
            })),
        );
    }
    {
        let b = backend.clone();
        bridge.register(
            "tool.terminal.cancel",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let b = b.clone();
                async move { handle_cancel(b, &ctx) }
            })),
        );
    }
    {
        let b = backend;
        bridge.register(
            "tool.terminal.tail",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let b = b.clone();
                async move { handle_tail(b, &ctx) }
            })),
        );
    }
}

/// PH-TERM-STREAM1: per-call cap on bytes returned by
/// `tool.terminal.tail`. Tighter than `MAX_OUTPUT_BYTES` so a
/// single tail response stays small; operator polls again with
/// `next_offset` when `truncated` is true.
const TAIL_PER_CALL_CAP: usize = 64 * 1024;

/// PH-TERM-STREAM1: `tool.terminal.tail` capability —
/// polling-cursor stream tail for live `tool.terminal.run`
/// sessions. The handler reads from the per-session stdout /
/// stderr buffer at the caller's offset and returns the new
/// chunk plus `next_offset`. Read-only; does NOT advance the
/// drainer's write head, so a > 1 MiB producer still stalls
/// once the buffer fills.
pub fn descriptor_tail() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.terminal.tail");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["shell:audit".into()];
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Polling stream tail for a live tool.terminal.run session. \
         Request JSON: {session_id, stream: \"stdout\"|\"stderr\", offset}. \
         Returns JSON: {session_id, stream, next_offset, chunk_bytes, \
         chunk (lossy-UTF-8), truncated}. Capped at 64 KiB per call; \
         operator polls again with next_offset when truncated. \
         INVALID_ARGS when the session id is unknown — fetch the final \
         output from the run response."
            .into(),
    );
    d.categories = vec!["read".into(), "terminal".into(), "streaming".into()];
    d.environment_requirements = vec!["shell:allowlist".into()];
    d
}

/// PH-TERM-CANCEL: `tool.terminal.cancel` capability —
/// cooperatively terminates a live `tool.terminal.run` session
/// by triggering its cancel notify. The run task observes the
/// notify, kills the child, and returns a `cancelled: true`
/// response. Idempotent: calling cancel on an already-completed
/// session returns INVALID_ARGS (session not present in the
/// registry); calling cancel twice in a row on the same live
/// session returns ok both times (the second call notifies a
/// notify whose waiter already left, which is a harmless no-op).
pub fn descriptor_cancel() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.terminal.cancel");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["shell:control".into()];
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Signal a running tool.terminal.run session to cancel. Arg is the \
         session id from tool.terminal.sessions. Returns `ok session=<id>` \
         on hit, INVALID_ARGS when the id is not present in the live \
         registry."
            .into(),
    );
    d.categories = vec!["mutate".into(), "terminal".into(), "control".into()];
    d.environment_requirements = vec!["shell:allowlist".into()];
    d
}

/// PH-TERM-AUDIT: `tool.terminal.audit_recent` capability —
/// bounded ring snapshot of completed runs. Pure in-memory
/// observability surface; defers to the dispatch-level audit
/// log for the cross-capability record.
pub fn descriptor_audit_recent() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.terminal.audit_recent");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["shell:audit".into()];
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Return the most recent completed tool.terminal.run invocations. \
         Arg is optional `<max>` (default 256). Tab-delim rows: \
         ts_secs\\tcommand\\texit_code\\tduration_ms\\ttimed_out\\tcaller_subject_id. \
         Newest first."
            .into(),
    );
    d.categories = vec!["read".into(), "terminal".into(), "audit".into()];
    d.environment_requirements = vec!["shell:allowlist".into()];
    d
}

/// PH-TERM-SESSIONS: `tool.terminal.sessions` capability —
/// snapshot of currently-running terminal invocations. Pure
/// in-memory observability surface. Useful for operators who
/// need to see whether a long-running spawn is still pending
/// before tearing the tool node down.
pub fn descriptor_sessions() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.terminal.sessions");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["shell:audit".into()];
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "List currently-running tool.terminal.run sessions. Tab-delim rows: \
         session_id\\tpid\\tcommand\\tstarted_at\\ttimeout_secs\\tcaller_subject_id. \
         Final row is `count=<N>`."
            .into(),
    );
    d.categories = vec!["read".into(), "terminal".into(), "audit".into()];
    d.environment_requirements = vec!["shell:allowlist".into()];
    d
}

async fn handle_run(backend: Arc<TerminalBackend>, ctx: InvocationCtx) -> HandlerOutcome {
    let req: RunRequest = match serde_json::from_slice(&ctx.args) {
        Ok(r) => r,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("tool.terminal.run: bad request shape: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    // Allowlist + path-traversal guards.
    if req.command.is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "tool.terminal.run: command required".to_string(),
            retry_hint: 2,
            retry_after: None,
        });
    }
    if req.command.contains('/') || req.command.contains('\\') {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!(
                "tool.terminal.run: command `{}` contains a path separator; \
                 only bare program names are accepted (operator allowlist \
                 enforces this)",
                req.command
            ),
            retry_hint: 2,
            retry_after: None,
        });
    }
    if !backend.allowed.contains(&req.command) {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::POLICY_DENIED,
            cause: format!(
                "tool.terminal.run: command `{}` is not in the operator's \
                 allowlist (allowed: {})",
                req.command,
                backend.cfg.allowed_commands.join(", ")
            ),
            retry_hint: 0,
            retry_after: None,
        });
    }
    // Effective timeout = min(per-call, cfg max). Always cap.
    let timeout_secs = req
        .timeout_secs
        .unwrap_or(backend.cfg.max_timeout_secs)
        .min(backend.cfg.max_timeout_secs)
        .max(1);
    let started = Instant::now();
    let mut command = tokio::process::Command::new(&req.command);
    command.args(&req.args);
    if !backend.cfg.inherit_env {
        command.env_clear();
        // Preserve a minimal PATH so the OS can find the
        // binary. Without this, env_clear breaks resolution
        // on Windows + many Unix setups.
        if let Ok(path) = std::env::var("PATH") {
            command.env("PATH", path);
        }
        if cfg!(windows) {
            if let Ok(p) = std::env::var("PATHEXT") {
                command.env("PATHEXT", p);
            }
            if let Ok(p) = std::env::var("SYSTEMROOT") {
                command.env("SYSTEMROOT", p);
            }
        }
    }
    if let Some(wd) = backend.cfg.working_dir.as_ref() {
        command.current_dir(wd);
    }
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    // Kill the child if its Child handle is dropped — defence
    // against runaway processes if the runtime panics mid-await.
    command.kill_on_drop(true);

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: format!("tool.terminal.run: spawn `{}` failed: {e}", req.command),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    let stdout_pipe = child
        .stdout
        .take()
        .expect("tool.terminal.run: stdout pipe present (was piped at spawn)");
    let stderr_pipe = child
        .stderr
        .take()
        .expect("tool.terminal.run: stderr pipe present (was piped at spawn)");

    // PH-TERM-SESSIONS / PH-TERM-CANCEL / PH-TERM-STREAM1:
    // register the live session BEFORE wiring up the race so a
    // cancel arriving anytime after spawn finds its target. The
    // stdout/stderr buffers live on the record so the
    // `tool.terminal.tail` poller can read them mid-run.
    let session_id = new_session_id();
    let pid = child.id();
    let cancel_notify = Arc::new(tokio::sync::Notify::new());
    let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    {
        let mut g = backend
            .sessions
            .lock()
            .expect("tool.terminal sessions poisoned");
        g.insert(
            session_id.clone(),
            TerminalSessionRecord {
                session_id: session_id.clone(),
                pid,
                command: req.command.clone(),
                args: req.args.clone(),
                started_at: unix_secs(),
                caller_subject_id: ctx.caller.subject_id.to_string(),
                timeout_secs,
                cancel_notify: cancel_notify.clone(),
                stdout_buf: stdout_buf.clone(),
                stderr_buf: stderr_buf.clone(),
            },
        );
    }

    // Drain stdout/stderr concurrently with the wait so the OS
    // pipe buffer never fills (which would block the child). The
    // drainers write through the shared Arc<Mutex<Vec<u8>>>;
    // `tool.terminal.tail` reads the same arcs.
    let stdout_drain = tokio::spawn(drain_pipe_into(
        stdout_pipe,
        stdout_buf.clone(),
        MAX_OUTPUT_BYTES,
    ));
    let stderr_drain = tokio::spawn(drain_pipe_into(
        stderr_pipe,
        stderr_buf.clone(),
        MAX_OUTPUT_BYTES,
    ));

    // Race wait against cancel + timeout. `biased` keeps the
    // ordering deterministic: a child that already exited wins
    // over a cancel that arrives at the same tick.
    let cancel_fut = cancel_notify.notified();
    tokio::pin!(cancel_fut);
    let timeout_fut = tokio::time::sleep(Duration::from_secs(timeout_secs));
    tokio::pin!(timeout_fut);
    let outcome = tokio::select! {
        biased;
        res = child.wait() => Termination::Exited(res),
        _ = &mut cancel_fut => {
            let _ = child.kill().await;
            Termination::Cancelled
        }
        _ = &mut timeout_fut => {
            let _ = child.kill().await;
            Termination::TimedOut
        }
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    // Drainers see EOF once the pipes close (child exit / kill);
    // join them so we have the final byte buffers in hand.
    let _ = stdout_drain.await;
    let _ = stderr_drain.await;
    // Drop the session record now that the child has terminated.
    {
        let mut g = backend
            .sessions
            .lock()
            .expect("tool.terminal sessions poisoned");
        g.remove(&session_id);
    }
    let stdout_bytes = std::mem::take(&mut *stdout_buf.lock().expect("stdout buf poisoned"));
    let stderr_bytes = std::mem::take(&mut *stderr_buf.lock().expect("stderr buf poisoned"));
    let (stdout, truncated_stdout) = truncate_output(stdout_bytes);
    let (stderr, truncated_stderr) = truncate_output(stderr_bytes);

    let (exit_code, timed_out, cancelled) = match outcome {
        Termination::Exited(Ok(status)) => (status.code(), false, false),
        Termination::Exited(Err(e)) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: format!("tool.terminal.run: wait failed: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
        Termination::TimedOut => (None, true, false),
        Termination::Cancelled => (None, false, true),
    };

    let resp = RunResponse {
        exit_code,
        stdout,
        stderr,
        duration_ms,
        timed_out,
        cancelled,
        truncated_stdout,
        truncated_stderr,
        command: req.command.clone(),
        timeout_secs,
    };
    if timed_out {
        tracing::warn!(
            caller = %ctx.caller.subject_id,
            command = %resp.command,
            timeout_secs,
            duration_ms,
            "tool.terminal.run timed out — child killed"
        );
    } else if cancelled {
        tracing::warn!(
            caller = %ctx.caller.subject_id,
            command = %resp.command,
            duration_ms,
            session_id = %session_id,
            "tool.terminal.run cancelled — child killed"
        );
    } else {
        tracing::info!(
            caller = %ctx.caller.subject_id,
            command = %resp.command,
            exit_code = ?resp.exit_code,
            duration_ms = resp.duration_ms,
            timeout_secs = resp.timeout_secs,
            truncated_stdout = resp.truncated_stdout,
            truncated_stderr = resp.truncated_stderr,
            "tool.terminal.run completed"
        );
    }
    // PH-TERM-AUDIT: record the terminated run on the ring. One
    // entry per run regardless of outcome; the response fields
    // disambiguate normal exit / timeout / cancel.
    backend.audit.push(TerminalAuditEntry {
        ts_secs: unix_secs(),
        command: resp.command.clone(),
        args: req.args.clone(),
        exit_code: resp.exit_code,
        duration_ms: resp.duration_ms,
        timed_out: resp.timed_out,
        cancelled: resp.cancelled,
        caller_subject_id: ctx.caller.subject_id.to_string(),
    });
    HandlerOutcome::Ok(serde_json::to_vec(&resp).unwrap_or_default())
}

/// PH-TERM-SESSIONS: handle `tool.terminal.sessions`. Returns
/// the live registry snapshot as tab-delim rows. Args are
/// ignored (operators may pass anything; the registration
/// validates nothing). Final row is `count=<N>`.
fn handle_sessions(backend: Arc<TerminalBackend>, _ctx: &InvocationCtx) -> HandlerOutcome {
    use std::fmt::Write as _;
    let mut sessions = backend.snapshot_sessions();
    // Stable order — newest first so paginated UIs render the
    // most-recent runs at the top.
    sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
    let count = sessions.len();
    let mut buf = String::new();
    for s in sessions {
        let safe_cmd = s.command.replace(['\t', '\n'], " ");
        let _ = writeln!(
            buf,
            "{}\t{}\t{}\t{}\t{}\t{}",
            s.session_id,
            s.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
            safe_cmd,
            s.started_at,
            s.timeout_secs,
            s.caller_subject_id,
        );
    }
    let _ = writeln!(buf, "count={count}");
    HandlerOutcome::Ok(buf.into_bytes())
}

/// PH-TERM-AUDIT: handle `tool.terminal.audit_recent`. Arg is
/// an optional decimal `<max>` (default 256, capped at ring
/// capacity). Returns one row per entry, newest first,
/// tab-delimited:
/// `ts_secs\tcommand\texit_code\tduration_ms\ttimed_out\tcancelled\tcaller_subject_id`.
/// Final row is `count=<N>`.
fn handle_audit_recent(backend: Arc<TerminalBackend>, ctx: &InvocationCtx) -> HandlerOutcome {
    use std::fmt::Write as _;
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("tool.terminal.audit_recent arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    let max = if s.is_empty() {
        TERMINAL_AUDIT_RING_DEFAULT
    } else {
        match s.parse::<usize>() {
            Ok(n) if n > 0 => n.min(TERMINAL_AUDIT_RING_DEFAULT),
            _ => {
                return HandlerOutcome::Err(ErrorEnvelope {
                    kind: error_kinds::INVALID_ARGS,
                    cause: format!(
                        "tool.terminal.audit_recent: arg must be a positive integer (got '{s}')"
                    ),
                    retry_hint: 2,
                    retry_after: None,
                });
            }
        }
    };
    let entries = backend.audit_snapshot(max);
    let count = entries.len();
    let mut buf = String::new();
    for e in entries {
        let safe_cmd = e.command.replace(['\t', '\n'], " ");
        let _ = writeln!(
            buf,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            e.ts_secs,
            safe_cmd,
            e.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into()),
            e.duration_ms,
            e.timed_out,
            e.cancelled,
            e.caller_subject_id,
        );
    }
    let _ = writeln!(buf, "count={count}");
    HandlerOutcome::Ok(buf.into_bytes())
}

/// PH-TERM-CANCEL: terminal-run termination cause.
enum Termination {
    /// Child exited (could be Ok status or wait IO error).
    Exited(std::io::Result<std::process::ExitStatus>),
    /// Hard timeout fired before the child exited; the run task
    /// killed the child.
    TimedOut,
    /// `tool.terminal.cancel` fired for this session; the run
    /// task killed the child.
    Cancelled,
}

/// PH-TERM-CANCEL: drain a piped stdio stream into a bounded
/// buffer. Reads until EOF (child closes the pipe), or until
/// the buffer hits `cap` bytes. Errors during read are treated
/// as EOF — a partial buffer is honest output for a kill.
async fn drain_pipe_into<R>(mut pipe: R, buf: Arc<Mutex<Vec<u8>>>, cap: usize)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let mut tmp = [0u8; 8192];
    loop {
        match pipe.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let mut g = buf.lock().expect("drain buf poisoned");
                if g.len() >= cap {
                    return;
                }
                let space = cap - g.len();
                let take = n.min(space);
                g.extend_from_slice(&tmp[..take]);
                if g.len() >= cap {
                    return;
                }
            }
        }
    }
}

/// PH-TERM-STREAM1: handle `tool.terminal.tail`. Request body
/// is JSON `{session_id, stream, offset}`. Response body is
/// JSON `{session_id, stream, next_offset, chunk_bytes, chunk,
/// truncated}`. INVALID_ARGS on unknown session / unknown
/// stream / malformed JSON.
fn handle_tail(backend: Arc<TerminalBackend>, ctx: &InvocationCtx) -> HandlerOutcome {
    #[derive(Debug, Deserialize)]
    struct TailRequest {
        session_id: String,
        stream: String,
        #[serde(default)]
        offset: u64,
    }
    #[derive(Debug, Serialize)]
    struct TailResponse {
        session_id: String,
        stream: String,
        next_offset: u64,
        chunk_bytes: usize,
        chunk: String,
        truncated: bool,
    }

    let req: TailRequest = match serde_json::from_slice(&ctx.args) {
        Ok(r) => r,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("tool.terminal.tail: bad request shape: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    if req.session_id.is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "tool.terminal.tail: session_id required".into(),
            retry_hint: 2,
            retry_after: None,
        });
    }
    let buf_arc = {
        let g = backend
            .sessions
            .lock()
            .expect("tool.terminal sessions poisoned");
        match g.get(&req.session_id) {
            Some(rec) => match req.stream.as_str() {
                "stdout" => rec.stdout_buf.clone(),
                "stderr" => rec.stderr_buf.clone(),
                other => {
                    return HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::INVALID_ARGS,
                        cause: format!(
                            "tool.terminal.tail: unknown stream '{other}'; use 'stdout' or 'stderr'"
                        ),
                        retry_hint: 0,
                        retry_after: None,
                    });
                }
            },
            None => {
                return HandlerOutcome::Err(ErrorEnvelope {
                    kind: error_kinds::INVALID_ARGS,
                    cause: format!(
                        "tool.terminal.tail: session not found (id='{}'); it may have already completed",
                        req.session_id
                    ),
                    retry_hint: 0,
                    retry_after: None,
                });
            }
        }
    };
    let (chunk_bytes, next_offset, truncated, chunk_str) = {
        let g = buf_arc.lock().expect("tool.terminal tail buf poisoned");
        let len = g.len();
        let start = (req.offset as usize).min(len);
        let mut end = len;
        let mut truncated = false;
        if end.saturating_sub(start) > TAIL_PER_CALL_CAP {
            end = start + TAIL_PER_CALL_CAP;
            truncated = true;
        }
        let chunk = &g[start..end];
        let chunk_str = String::from_utf8_lossy(chunk).into_owned();
        (chunk.len(), end as u64, truncated, chunk_str)
    };
    let resp = TailResponse {
        session_id: req.session_id,
        stream: req.stream,
        next_offset,
        chunk_bytes,
        chunk: chunk_str,
        truncated,
    };
    HandlerOutcome::Ok(serde_json::to_vec(&resp).unwrap_or_default())
}

/// PH-TERM-CANCEL: handle `tool.terminal.cancel`. Arg is the
/// session id from `tool.terminal.sessions`. Looks the session
/// up in the live registry and triggers its cancel notify.
/// Returns `ok session=<id>\n` on hit, INVALID_ARGS otherwise.
fn handle_cancel(backend: Arc<TerminalBackend>, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("tool.terminal.cancel arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    if s.is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "tool.terminal.cancel: session_id required".into(),
            retry_hint: 2,
            retry_after: None,
        });
    }
    let notify = {
        let g = backend
            .sessions
            .lock()
            .expect("tool.terminal sessions poisoned");
        g.get(s).map(|r| r.cancel_notify.clone())
    };
    match notify {
        Some(n) => {
            // notify_one() stores a permit even if the awaiter
            // hasn't started yet, so a cancel that arrives
            // moments after spawn (between register and select!)
            // is not lost.
            n.notify_one();
            HandlerOutcome::Ok(format!("ok session={s}\n").into_bytes())
        }
        None => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!(
                "tool.terminal.cancel: session not found (id='{s}'); it may have already completed"
            ),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

/// PH-TERM-SESSIONS: 16 hex chars of randomness — matches the
/// existing CW4 browser session id shape so the operator UX
/// is consistent across session-bearing capabilities.
fn new_session_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Truncate a byte buffer to `MAX_OUTPUT_BYTES`, returning
/// the lossy-UTF-8 string + a flag. Uses `from_utf8_lossy`
/// because operators want SOMETHING readable for diagnostic
/// purposes; the bridge audit also records the raw bytes
/// (caps + flags surface in the response so callers know
/// when they need to re-run with a different capture path).
fn truncate_output(mut bytes: Vec<u8>) -> (String, bool) {
    let truncated = bytes.len() > MAX_OUTPUT_BYTES;
    if truncated {
        bytes.truncate(MAX_OUTPUT_BYTES);
    }
    (String::from_utf8_lossy(&bytes).into_owned(), truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(allowed: &[&str]) -> TerminalConfig {
        TerminalConfig {
            allowed_commands: allowed.iter().map(|s| s.to_string()).collect(),
            max_timeout_secs: 30,
            inherit_env: false,
            working_dir: None,
        }
    }

    #[test]
    fn backend_construction_rejects_empty_allowlist() {
        let err = TerminalBackend::new(cfg(&[])).unwrap_err();
        assert!(err.contains("fails closed"));
    }

    #[test]
    fn backend_construction_rejects_path_traversal_in_allowlist() {
        let err = TerminalBackend::new(cfg(&["bin/ls"])).unwrap_err();
        assert!(err.contains("path separator"));
        let err = TerminalBackend::new(cfg(&["C:\\bin\\cmd"])).unwrap_err();
        assert!(err.contains("path separator"));
    }

    #[test]
    fn backend_construction_rejects_zero_timeout() {
        let mut c = cfg(&["echo"]);
        c.max_timeout_secs = 0;
        let err = TerminalBackend::new(c).unwrap_err();
        assert!(err.contains("max_timeout_secs"));
    }

    #[test]
    fn backend_construction_rejects_empty_entry() {
        let err = TerminalBackend::new(cfg(&[""])).unwrap_err();
        assert!(err.contains("empty entry"));
    }

    #[test]
    fn backend_normalizes_allowlist_to_set_for_lookup() {
        let b = TerminalBackend::new(cfg(&["echo", "ls", "echo"])).unwrap();
        // Dedup via BTreeSet.
        assert_eq!(b.allowed.len(), 2);
        assert!(b.allowed.contains("echo"));
        assert!(b.allowed.contains("ls"));
    }

    #[test]
    fn truncate_output_caps_at_max_and_flags() {
        let big = vec![b'a'; MAX_OUTPUT_BYTES + 100];
        let (s, truncated) = truncate_output(big);
        assert_eq!(s.len(), MAX_OUTPUT_BYTES);
        assert!(truncated);
    }

    #[test]
    fn truncate_output_passes_through_when_within_cap() {
        let small = b"hello".to_vec();
        let (s, truncated) = truncate_output(small);
        assert_eq!(s, "hello");
        assert!(!truncated);
    }

    // ── PH-TERM-SESSIONS: live run registry + tool.terminal.sessions ──

    #[test]
    fn sessions_descriptor_shape() {
        let d = descriptor_sessions();
        assert_eq!(d.method_name, "tool.terminal.sessions");
        assert_eq!(d.major_version, 1);
        assert!(matches!(d.idempotency, Idempotency::Idempotent));
        assert!(matches!(d.cost_class, CostClass::Cheap));
        assert!(d.sensitivity_tags.iter().any(|t| t == "shell:audit"));
        assert!(d.requires_groups.iter().any(|g| g == "operators"));
    }

    #[test]
    fn fresh_backend_has_no_sessions() {
        let b = TerminalBackend::new(cfg(&["echo"])).unwrap();
        assert_eq!(b.snapshot_sessions().len(), 0);
    }

    #[test]
    fn new_session_id_is_16_hex_chars() {
        let id = new_session_id();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
        // Different draw, different id (collision probability
        // is 2^-64, so a different value is overwhelmingly likely).
        assert_ne!(new_session_id(), id);
    }

    #[test]
    fn handle_sessions_returns_count_zero_when_empty() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let ctx = test_ctx();
        let r = handle_sessions(b, &ctx);
        let body = match r {
            HandlerOutcome::Ok(bytes) => String::from_utf8(bytes).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        assert_eq!(body.trim(), "count=0");
    }

    fn mk_session_record(id: &str, started_at: i64, command: &str) -> TerminalSessionRecord {
        TerminalSessionRecord {
            session_id: id.into(),
            pid: Some(42),
            command: command.into(),
            args: vec![],
            started_at,
            caller_subject_id: "deadbeef".into(),
            timeout_secs: 30,
            cancel_notify: Arc::new(tokio::sync::Notify::new()),
            stdout_buf: Arc::new(Mutex::new(Vec::new())),
            stderr_buf: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[test]
    fn snapshot_reflects_manual_insert_and_remove() {
        let b = TerminalBackend::new(cfg(&["echo"])).unwrap();
        // Manually insert to exercise the snapshot path
        // without spawning a real process.
        let rec = mk_session_record("abc123", 1_700_000_000, "echo");
        b.sessions.lock().unwrap().insert("abc123".into(), rec);
        let snap = b.snapshot_sessions();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].session_id, "abc123");
        assert_eq!(snap[0].pid, Some(42));

        b.sessions.lock().unwrap().remove("abc123");
        assert_eq!(b.snapshot_sessions().len(), 0);
    }

    #[test]
    fn handle_sessions_formats_rows_newest_first_with_count() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        // Insert two sessions with different started_at so the
        // ordering assertion is meaningful.
        {
            let mut g = b.sessions.lock().unwrap();
            g.insert("old".into(), mk_session_record("old", 100, "echo"));
            g.insert("new".into(), mk_session_record("new", 200, "ls"));
        }
        let ctx = test_ctx();
        let r = handle_sessions(b, &ctx);
        let body = match r {
            HandlerOutcome::Ok(bytes) => String::from_utf8(bytes).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("new\t"), "first row: {}", lines[0]);
        assert!(lines[1].starts_with("old\t"), "second row: {}", lines[1]);
        assert_eq!(lines[2], "count=2");
    }

    fn test_ctx() -> InvocationCtx {
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
            args: Vec::new(),
        }
    }

    // ── PH-TERM-AUDIT: completed-run audit ring ────────────────────

    fn ctx_with_args(args: &[u8]) -> InvocationCtx {
        let mut c = test_ctx();
        c.args = args.to_vec();
        c
    }

    #[test]
    fn audit_recent_descriptor_shape() {
        let d = descriptor_audit_recent();
        assert_eq!(d.method_name, "tool.terminal.audit_recent");
        assert!(matches!(d.idempotency, Idempotency::Idempotent));
        assert!(matches!(d.cost_class, CostClass::Cheap));
        assert!(d.sensitivity_tags.iter().any(|t| t == "shell:audit"));
        assert!(d.requires_groups.iter().any(|g| g == "operators"));
    }

    #[test]
    fn fresh_backend_audit_ring_is_empty() {
        let b = TerminalBackend::new(cfg(&["echo"])).unwrap();
        assert_eq!(b.audit_snapshot(10).len(), 0);
    }

    #[test]
    fn audit_ring_bounded_by_capacity() {
        let b = TerminalBackend::new(cfg(&["echo"])).unwrap();
        for i in 0..(TERMINAL_AUDIT_RING_DEFAULT + 10) {
            b.audit.push(TerminalAuditEntry {
                ts_secs: i as i64,
                command: format!("e{i}"),
                args: vec![],
                exit_code: Some(0),
                duration_ms: 1,
                timed_out: false,
                cancelled: false,
                caller_subject_id: "x".into(),
            });
        }
        assert_eq!(b.audit_snapshot(10_000).len(), TERMINAL_AUDIT_RING_DEFAULT);
    }

    #[test]
    fn audit_ring_snapshot_is_newest_first() {
        let b = TerminalBackend::new(cfg(&["echo"])).unwrap();
        for i in 0..3 {
            b.audit.push(TerminalAuditEntry {
                ts_secs: i as i64,
                command: format!("e{i}"),
                args: vec![],
                exit_code: Some(0),
                duration_ms: 1,
                timed_out: false,
                cancelled: false,
                caller_subject_id: "x".into(),
            });
        }
        let snap = b.audit_snapshot(10);
        assert_eq!(snap[0].command, "e2");
        assert_eq!(snap[1].command, "e1");
        assert_eq!(snap[2].command, "e0");
    }

    #[test]
    fn handle_audit_recent_empty_returns_count_zero() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let ctx = test_ctx();
        let r = handle_audit_recent(b, &ctx);
        let body = match r {
            HandlerOutcome::Ok(bytes) => String::from_utf8(bytes).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        assert_eq!(body.trim(), "count=0");
    }

    #[test]
    fn handle_audit_recent_formats_rows_with_count() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        b.audit.push(TerminalAuditEntry {
            ts_secs: 100,
            command: "echo".into(),
            args: vec!["hi".into()],
            exit_code: Some(0),
            duration_ms: 5,
            timed_out: false,
            cancelled: false,
            caller_subject_id: "aa".into(),
        });
        b.audit.push(TerminalAuditEntry {
            ts_secs: 200,
            command: "ls".into(),
            args: vec![],
            exit_code: None,
            duration_ms: 30000,
            timed_out: true,
            cancelled: false,
            caller_subject_id: "bb".into(),
        });
        let ctx = test_ctx();
        let r = handle_audit_recent(b, &ctx);
        let body = match r {
            HandlerOutcome::Ok(bytes) => String::from_utf8(bytes).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        // Newest first — ts 200 row before ts 100 row. Format:
        // ts\tcommand\texit_code\tduration_ms\ttimed_out\tcancelled\tcaller.
        assert!(lines[0].starts_with("200\tls\t?\t30000\ttrue\tfalse\tbb"));
        assert!(lines[1].starts_with("100\techo\t0\t5\tfalse\tfalse\taa"));
        assert_eq!(lines[2], "count=2");
    }

    #[test]
    fn handle_audit_recent_respects_max_arg() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        for i in 0..5 {
            b.audit.push(TerminalAuditEntry {
                ts_secs: i,
                command: format!("e{i}"),
                args: vec![],
                exit_code: Some(0),
                duration_ms: 1,
                timed_out: false,
                cancelled: false,
                caller_subject_id: "x".into(),
            });
        }
        let ctx = ctx_with_args(b"2");
        let r = handle_audit_recent(b, &ctx);
        let body = match r {
            HandlerOutcome::Ok(bytes) => String::from_utf8(bytes).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2], "count=2");
    }

    #[test]
    fn handle_audit_recent_rejects_non_numeric_arg() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let ctx = ctx_with_args(b"abc");
        let r = handle_audit_recent(b, &ctx);
        match r {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("positive integer")),
            _ => panic!("expected Err"),
        }
    }

    // ── PH-TERM-CANCEL: tool.terminal.cancel ───────────────────────

    #[test]
    fn cancel_descriptor_shape() {
        let d = descriptor_cancel();
        assert_eq!(d.method_name, "tool.terminal.cancel");
        assert!(matches!(d.idempotency, Idempotency::Idempotent));
        assert!(matches!(d.cost_class, CostClass::Cheap));
        assert!(d.sensitivity_tags.iter().any(|t| t == "shell:control"));
        assert!(d.requires_groups.iter().any(|g| g == "operators"));
    }

    #[test]
    fn cancel_empty_arg_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let ctx = ctx_with_args(b"");
        match handle_cancel(b, &ctx) {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("session_id required")),
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn cancel_unknown_session_returns_invalid_args() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let ctx = ctx_with_args(b"deadbeef0000");
        match handle_cancel(b, &ctx) {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("session not found"));
                assert_eq!(e.kind, relix_core::types::error_kinds::INVALID_ARGS);
            }
            _ => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn cancel_known_session_triggers_notify() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let session_id = "session-abc".to_string();
        let notify = Arc::new(tokio::sync::Notify::new());
        {
            let mut g = b.sessions.lock().unwrap();
            let mut rec = mk_session_record(&session_id, unix_secs(), "echo");
            rec.cancel_notify = notify.clone();
            g.insert(session_id.clone(), rec);
        }
        // Set up an awaiter on the same notify BEFORE issuing
        // cancel, so we can prove the wakeup actually delivers.
        let awaited = notify.clone();
        let handle = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(2), awaited.notified())
                .await
                .is_ok()
        });
        // Issue cancel. Use a brief yield to let the awaiter
        // register its waker; notify_one's stored permit also
        // covers the race, but the await round-trip exercises
        // the wakeup path either way.
        tokio::task::yield_now().await;
        let ctx = ctx_with_args(session_id.as_bytes());
        match handle_cancel(b.clone(), &ctx) {
            HandlerOutcome::Ok(bytes) => {
                let s = String::from_utf8(bytes).unwrap();
                assert!(s.contains(&format!("ok session={session_id}")));
            }
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        }
        let observed = handle.await.unwrap();
        assert!(observed, "awaiter should have observed the cancel notify");
    }

    #[tokio::test]
    async fn cancel_uses_notify_one_so_permit_survives_no_awaiter() {
        // notify_one() stores a permit; the next notified()
        // future resolves immediately. This protects against
        // the race between session register and the run task's
        // select! creating its notified() future.
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let session_id = "session-xyz".to_string();
        let notify = Arc::new(tokio::sync::Notify::new());
        {
            let mut g = b.sessions.lock().unwrap();
            let mut rec = mk_session_record(&session_id, unix_secs(), "echo");
            rec.cancel_notify = notify.clone();
            g.insert(session_id.clone(), rec);
        }
        // Cancel BEFORE any awaiter exists.
        let ctx = ctx_with_args(session_id.as_bytes());
        match handle_cancel(b.clone(), &ctx) {
            HandlerOutcome::Ok(_) => {}
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        }
        // Now create an awaiter — it must resolve immediately
        // because notify_one stored a permit.
        let n = notify.clone();
        let got = tokio::time::timeout(Duration::from_millis(500), n.notified())
            .await
            .is_ok();
        assert!(got, "stored permit should fire immediately");
    }

    #[tokio::test]
    async fn drain_pipe_into_caps_at_capacity() {
        // Feed more than MAX_OUTPUT_BYTES through a tokio duplex
        // and verify the drainer stops at the cap rather than
        // growing unbounded.
        use tokio::io::AsyncWriteExt;
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let drain_handle = tokio::spawn(drain_pipe_into(reader, buf.clone(), MAX_OUTPUT_BYTES));
        let chunk = vec![b'a'; 8192];
        let mut written = 0usize;
        while written < MAX_OUTPUT_BYTES + 16_384 {
            if writer.write_all(&chunk).await.is_err() {
                break;
            }
            written += chunk.len();
        }
        drop(writer);
        let _ = drain_handle.await;
        assert_eq!(buf.lock().unwrap().len(), MAX_OUTPUT_BYTES);
    }

    #[tokio::test]
    async fn drain_pipe_into_stops_on_eof_below_cap() {
        use tokio::io::AsyncWriteExt;
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let drain_handle = tokio::spawn(drain_pipe_into(reader, buf.clone(), MAX_OUTPUT_BYTES));
        writer.write_all(b"hello world\n").await.unwrap();
        drop(writer);
        drain_handle.await.unwrap();
        assert_eq!(buf.lock().unwrap().as_slice(), b"hello world\n");
    }

    // ── PH-TERM-STREAM1: tool.terminal.tail ────────────────────────

    fn insert_session_with_bytes(b: &Arc<TerminalBackend>, id: &str, stdout: &[u8], stderr: &[u8]) {
        let rec = mk_session_record(id, unix_secs(), "echo");
        rec.stdout_buf.lock().unwrap().extend_from_slice(stdout);
        rec.stderr_buf.lock().unwrap().extend_from_slice(stderr);
        b.sessions.lock().unwrap().insert(id.into(), rec);
    }

    fn parse_tail(body: &[u8]) -> serde_json::Value {
        serde_json::from_slice(body).expect("tail response is JSON")
    }

    #[test]
    fn tail_descriptor_shape() {
        let d = descriptor_tail();
        assert_eq!(d.method_name, "tool.terminal.tail");
        assert!(matches!(d.idempotency, Idempotency::Idempotent));
        assert!(matches!(d.cost_class, CostClass::Cheap));
        assert!(d.sensitivity_tags.iter().any(|t| t == "shell:audit"));
        assert!(d.requires_groups.iter().any(|g| g == "operators"));
        assert!(d.categories.iter().any(|c| c == "streaming"));
    }

    #[test]
    fn tail_bad_json_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let ctx = ctx_with_args(b"not-json");
        match handle_tail(b, &ctx) {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("bad request shape"));
                assert_eq!(e.kind, error_kinds::INVALID_ARGS);
            }
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn tail_empty_session_id_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let arg = br#"{"session_id":"","stream":"stdout","offset":0}"#;
        match handle_tail(b, &ctx_with_args(arg)) {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("session_id required")),
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn tail_unknown_session_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let arg = br#"{"session_id":"abc123","stream":"stdout","offset":0}"#;
        match handle_tail(b, &ctx_with_args(arg)) {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("session not found")),
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn tail_unknown_stream_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        insert_session_with_bytes(&b, "s1", b"hello", b"");
        let arg = br#"{"session_id":"s1","stream":"banana","offset":0}"#;
        match handle_tail(b, &ctx_with_args(arg)) {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("unknown stream")),
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn tail_returns_full_chunk_from_offset_zero() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        insert_session_with_bytes(&b, "s1", b"hello world", b"");
        let arg = br#"{"session_id":"s1","stream":"stdout","offset":0}"#;
        let body = match handle_tail(b, &ctx_with_args(arg)) {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let v = parse_tail(&body);
        assert_eq!(v["chunk"], "hello world");
        assert_eq!(v["chunk_bytes"], 11);
        assert_eq!(v["next_offset"], 11);
        assert_eq!(v["truncated"], false);
        assert_eq!(v["stream"], "stdout");
        assert_eq!(v["session_id"], "s1");
    }

    #[test]
    fn tail_returns_slice_from_mid_offset() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        insert_session_with_bytes(&b, "s1", b"abcdefghij", b"");
        let arg = br#"{"session_id":"s1","stream":"stdout","offset":3}"#;
        let body = match handle_tail(b, &ctx_with_args(arg)) {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let v = parse_tail(&body);
        assert_eq!(v["chunk"], "defghij");
        assert_eq!(v["chunk_bytes"], 7);
        assert_eq!(v["next_offset"], 10);
    }

    #[test]
    fn tail_offset_past_end_returns_empty_chunk() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        insert_session_with_bytes(&b, "s1", b"abc", b"");
        let arg = br#"{"session_id":"s1","stream":"stdout","offset":99}"#;
        let body = match handle_tail(b, &ctx_with_args(arg)) {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let v = parse_tail(&body);
        assert_eq!(v["chunk"], "");
        assert_eq!(v["chunk_bytes"], 0);
        // next_offset clamps to current buffer end so a stale
        // caller can self-correct.
        assert_eq!(v["next_offset"], 3);
        assert_eq!(v["truncated"], false);
    }

    #[test]
    fn tail_truncates_at_per_call_cap() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let big = vec![b'a'; TAIL_PER_CALL_CAP + 5000];
        insert_session_with_bytes(&b, "s1", &big, b"");
        let arg = br#"{"session_id":"s1","stream":"stdout","offset":0}"#;
        let body = match handle_tail(b, &ctx_with_args(arg)) {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let v = parse_tail(&body);
        assert_eq!(v["chunk_bytes"], TAIL_PER_CALL_CAP);
        assert_eq!(v["next_offset"], TAIL_PER_CALL_CAP);
        assert_eq!(v["truncated"], true);
    }

    #[test]
    fn tail_stderr_independent_from_stdout() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        insert_session_with_bytes(&b, "s1", b"out", b"ERR");
        let arg = br#"{"session_id":"s1","stream":"stderr","offset":0}"#;
        let body = match handle_tail(b, &ctx_with_args(arg)) {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let v = parse_tail(&body);
        assert_eq!(v["chunk"], "ERR");
        assert_eq!(v["stream"], "stderr");
    }

    #[test]
    fn tail_offset_default_is_zero_when_omitted() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        insert_session_with_bytes(&b, "s1", b"hi", b"");
        // No `offset` field — should default to 0.
        let arg = br#"{"session_id":"s1","stream":"stdout"}"#;
        let body = match handle_tail(b, &ctx_with_args(arg)) {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let v = parse_tail(&body);
        assert_eq!(v["chunk"], "hi");
        assert_eq!(v["next_offset"], 2);
    }
}
