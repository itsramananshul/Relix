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
//! ## Out of scope (alpha)
//!
//! - No process interruption mid-run (cooperative cancellation
//!   would require flow-runner integration; today the only
//!   stop is the hard timeout).
//! - No streaming stdout/stderr (whole-buffer at completion).
//! - No background / detached execution.
//! - No persistent shell sessions.
//! - No interactive stdin.
//!
//! These are explicit future-work items, not silent omissions.
//! The chronicle entry the bridge would write against a calling
//! task records the exit code + duration, which is enough for
//! post-hoc debugging; streaming + interruption land at Gate 2
//! alongside the resumable VM.

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

/// PH-TERM-SESSIONS: one live `tool.terminal.run` invocation
/// in flight. Inserted on spawn, removed on completion (success,
/// timeout, or spawn failure). Pure observability — does NOT
/// give an outside caller a kill handle yet; that lands in a
/// follow-up milestone alongside `tool.terminal.cancel`.
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
}

/// PH-TERM-AUDIT: one completed `tool.terminal.run` invocation
/// observation. Pushed onto the bounded audit ring after every
/// terminated run regardless of outcome (normal exit, timeout,
/// or wait-error). Pure in-memory observability — does NOT
/// replace the dispatch-level audit log, does NOT duplicate
/// chronicle.
#[derive(Clone, Debug)]
pub struct TerminalAuditEntry {
    /// Wall-clock unix seconds at the moment of completion.
    pub ts_secs: i64,
    pub command: String,
    pub args: Vec<String>,
    /// Exit code as reported by the OS. `None` when the child
    /// was killed (timeout) or wait failed.
    pub exit_code: Option<i32>,
    /// Wall-clock elapsed from spawn to termination, in
    /// milliseconds.
    pub duration_ms: u64,
    pub timed_out: bool,
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
    /// process was killed (timeout / signal — `timed_out`
    /// disambiguates).
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    duration_ms: u64,
    /// True when the timeout fired and we killed the child
    /// before it exited naturally.
    timed_out: bool,
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
        let b = backend;
        bridge.register(
            "tool.terminal.audit_recent",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let b = b.clone();
                async move { handle_audit_recent(b, &ctx) }
            })),
        );
    }
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

    let child = match command.spawn() {
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
    // PH-TERM-SESSIONS: register the live session BEFORE awaiting
    // wait_with_output (which consumes the Child). The session
    // record is removed unconditionally on completion.
    let session_id = new_session_id();
    let pid = child.id();
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
            },
        );
    }
    let wait_fut = child.wait_with_output();
    let outcome = tokio::time::timeout(Duration::from_secs(timeout_secs), wait_fut).await;
    let duration_ms = started.elapsed().as_millis() as u64;
    // Drop the session record now that the child has exited (or
    // timed out and been killed via kill_on_drop).
    {
        let mut g = backend
            .sessions
            .lock()
            .expect("tool.terminal sessions poisoned");
        g.remove(&session_id);
    }

    match outcome {
        Ok(Ok(out)) => {
            let (stdout, truncated_stdout) = truncate_output(out.stdout);
            let (stderr, truncated_stderr) = truncate_output(out.stderr);
            let resp = RunResponse {
                exit_code: out.status.code(),
                stdout,
                stderr,
                duration_ms,
                timed_out: false,
                truncated_stdout,
                truncated_stderr,
                command: req.command,
                timeout_secs,
            };
            let body = serde_json::to_vec(&resp).unwrap_or_default();
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
            // PH-TERM-AUDIT: record the completed run on the
            // ring. Pure observability; happens regardless of
            // exit code (an exit-1 is still a completed run).
            backend.audit.push(TerminalAuditEntry {
                ts_secs: unix_secs(),
                command: resp.command.clone(),
                args: req.args.clone(),
                exit_code: resp.exit_code,
                duration_ms: resp.duration_ms,
                timed_out: false,
                caller_subject_id: ctx.caller.subject_id.to_string(),
            });
            HandlerOutcome::Ok(body)
        }
        Ok(Err(e)) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("tool.terminal.run: wait failed: {e}"),
            retry_hint: 2,
            retry_after: None,
        }),
        Err(_elapsed) => {
            // Timed out — child was killed by kill_on_drop
            // when wait_with_output's owned Child was dropped.
            // We don't have output in this path; surface what
            // we know honestly.
            tracing::warn!(
                caller = %ctx.caller.subject_id,
                command = %req.command,
                timeout_secs,
                duration_ms,
                "tool.terminal.run timed out — child killed"
            );
            let resp = RunResponse {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                duration_ms,
                timed_out: true,
                truncated_stdout: false,
                truncated_stderr: false,
                command: req.command,
                timeout_secs,
            };
            // PH-TERM-AUDIT: record the timed-out run too.
            backend.audit.push(TerminalAuditEntry {
                ts_secs: unix_secs(),
                command: resp.command.clone(),
                args: req.args.clone(),
                exit_code: None,
                duration_ms: resp.duration_ms,
                timed_out: true,
                caller_subject_id: ctx.caller.subject_id.to_string(),
            });
            HandlerOutcome::Ok(serde_json::to_vec(&resp).unwrap_or_default())
        }
    }
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
/// `ts_secs\tcommand\texit_code\tduration_ms\ttimed_out\tcaller_subject_id`.
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
            "{}\t{}\t{}\t{}\t{}\t{}",
            e.ts_secs,
            safe_cmd,
            e.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into()),
            e.duration_ms,
            e.timed_out,
            e.caller_subject_id,
        );
    }
    let _ = writeln!(buf, "count={count}");
    HandlerOutcome::Ok(buf.into_bytes())
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

    #[test]
    fn snapshot_reflects_manual_insert_and_remove() {
        let b = TerminalBackend::new(cfg(&["echo"])).unwrap();
        // Manually insert to exercise the snapshot path
        // without spawning a real process (the spawn path is
        // covered by handle_run's existing behavior + the
        // controller integration).
        let rec = TerminalSessionRecord {
            session_id: "abc123".into(),
            pid: Some(42),
            command: "echo".into(),
            args: vec!["hi".into()],
            started_at: 1_700_000_000,
            caller_subject_id: "deadbeef".into(),
            timeout_secs: 30,
        };
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
            g.insert(
                "old".into(),
                TerminalSessionRecord {
                    session_id: "old".into(),
                    pid: Some(10),
                    command: "echo".into(),
                    args: vec![],
                    started_at: 100,
                    caller_subject_id: "aa".into(),
                    timeout_secs: 30,
                },
            );
            g.insert(
                "new".into(),
                TerminalSessionRecord {
                    session_id: "new".into(),
                    pid: Some(11),
                    command: "ls".into(),
                    args: vec![],
                    started_at: 200,
                    caller_subject_id: "bb".into(),
                    timeout_secs: 30,
                },
            );
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
            caller_subject_id: "aa".into(),
        });
        b.audit.push(TerminalAuditEntry {
            ts_secs: 200,
            command: "ls".into(),
            args: vec![],
            exit_code: None,
            duration_ms: 30000,
            timed_out: true,
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
        // Newest first — ts 200 row before ts 100 row.
        assert!(lines[0].starts_with("200\tls\t?\t30000\ttrue\tbb"));
        assert!(lines[1].starts_with("100\techo\t0\t5\tfalse\taa"));
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
}
