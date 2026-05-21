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
//! ## Background execution (PH-TERM-SPAWN)
//!
//! `tool.terminal.spawn` is the fire-and-forget sibling of
//! `tool.terminal.run`. Same validation + spawn posture; the
//! response carries `{session_id, pid, command, timeout_secs,
//! started_at}` and returns immediately while the run continues
//! asynchronously on the tokio runtime. The completion path is
//! identical to `run` — same audit ring push, same session
//! cleanup, same `tail` visibility — so consuming a backgrounded
//! run uses the same surfaces as the synchronous one.
//!
//! ## Persistent shell sessions (PH-TERM-SHELL)
//!
//! `tool.terminal.shell.open` spawns a shell from a separate
//! operator-managed allowlist (`allowed_shells`) with stdin
//! piped, stdout/stderr drained into the same per-session
//! buffers that `tool.terminal.tail` reads. Operators send
//! bytes to stdin via `tool.terminal.shell.input` and close the
//! stdin pipe (signalling EOF) via `tool.terminal.shell.close`.
//! The shell process is otherwise tracked exactly like a
//! background run — `tool.terminal.sessions` lists it,
//! `tool.terminal.cancel` kills it, `tool.terminal.audit_recent`
//! records the eventual exit.
//!
//! Honest limitation: there is NO command-boundary tracking
//! inside the shell. Output from one `input` call is
//! interleaved with output from prior calls in the same stdout
//! buffer; operators who need per-command exit codes inject
//! their own sentinel (e.g., `echo "__RELIX_DONE_$?__"`).
//!
//! ## Interactive stdin (PH-TERM-CONTROL)
//!
//! `tool.terminal.shell.control` writes a named control byte
//! sequence (etx / eot / tab / cr / lf / enter / esc /
//! backspace / sub / nak) to a session's stdin so operators do
//! not have to know the byte values or base64-encode them.
//!
//! **Honest limitation: no PTY.** The current shell sessions
//! attach the child's stdin to a regular OS pipe, NOT to a
//! pseudo-terminal. Programs that check `isatty()` will see
//! `false` and switch to non-interactive mode; programs that
//! rely on the TTY driver to translate control bytes into
//! signals (e.g., interactive bash translating `0x03` into
//! SIGINT to the foreground command) will NOT receive those
//! signals — the bytes arrive on stdin as ordinary input. A
//! future PH-TERM-PTY milestone would allocate a real PTY via
//! the `portable-pty` crate; the architecture mismatch with
//! `tokio::process::Child` puts it outside this milestone's
//! scope.
//!
//! ## Still out of scope (alpha)
//!
//! - No streaming-with-consumer-drain (tail is read-only; it
//!   does NOT advance the drainer's write head, so a long-
//!   running run producing > 1 MiB still stalls).
//! - No command-boundary tracking inside a persistent shell
//!   (the shell session is a single bytes-in / bytes-out
//!   stream — operators don't get per-command exit codes
//!   without their own sentinel).
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

use relix_core::capability::{CapabilityDescriptor, CostClass, Idempotency, RiskLevel};
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
    /// PH-TERM-SHELL: bare program names the operator has
    /// allowed for persistent shell sessions via
    /// `tool.terminal.shell.open`. Separate from
    /// `allowed_commands` so operators can opt into bash /
    /// powershell without opening every command name to spawn.
    /// Default `[]` — shell sessions are gated until the
    /// operator explicitly lists at least one shell binary.
    #[serde(default)]
    pub allowed_shells: Vec<String>,
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
    /// PH-TERM-SHELL: shell allowlist (separate from
    /// `allowed`). Empty when no shells are allowed — the open
    /// handler refuses fail-closed in that case.
    allowed_shells: BTreeSet<String>,
    /// PH-TERM-SESSIONS: live in-flight runs. Keyed by the
    /// session id allocated at spawn. Mutex-bounded; held only
    /// for short insert/remove/snapshot transactions.
    sessions: Mutex<HashMap<String, TerminalSessionRecord>>,
    /// PH-TERM-AUDIT: bounded ring of completed runs.
    audit: TerminalAuditRing,
    /// PH-TERM-SHELL: per-session stdin writers for persistent
    /// shell sessions. Parallel to `sessions` (same keying);
    /// insert on `tool.terminal.shell.open`, remove on
    /// `tool.terminal.shell.close` or when
    /// [`drive_to_completion`] cleans up the session. Wrapped in
    /// `tokio::sync::Mutex` so the input handler can hold the
    /// guard across the `write_all` / `flush` awaits.
    shell_stdins:
        Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<tokio::process::ChildStdin>>>>>,
}

impl TerminalBackend {
    pub fn new(cfg: TerminalConfig) -> Result<Self, String> {
        // PH-TERM-SHELL: either allowlist may be empty, but not
        // both — at least one mode of execution must be enabled.
        if cfg.allowed_commands.is_empty() && cfg.allowed_shells.is_empty() {
            return Err("tool.terminal: at least one of `allowed_commands` or \
                 `allowed_shells` must list a binary; the capability fails \
                 closed when no allowlist is provided"
                .to_string());
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
        // PH-TERM-SHELL: shell allowlist validation. Empty is
        // acceptable (shell sessions disabled); a populated list
        // must follow the same bare-program-name rules as
        // allowed_commands.
        for shell in &cfg.allowed_shells {
            if shell.is_empty() {
                return Err("tool.terminal: allowed_shells contains empty entry".to_string());
            }
            if shell.contains('/') || shell.contains('\\') {
                return Err(format!(
                    "tool.terminal: allowed_shells entry `{shell}` contains a path \
                     separator; only bare program names are accepted"
                ));
            }
        }
        let allowed: BTreeSet<String> = cfg.allowed_commands.iter().cloned().collect();
        let allowed_shells: BTreeSet<String> = cfg.allowed_shells.iter().cloned().collect();
        Ok(Self {
            cfg,
            allowed,
            allowed_shells,
            sessions: Mutex::new(HashMap::new()),
            audit: TerminalAuditRing::new(TERMINAL_AUDIT_RING_DEFAULT),
            shell_stdins: Mutex::new(HashMap::new()),
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
        let b = backend.clone();
        bridge.register(
            "tool.terminal.tail",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let b = b.clone();
                async move { handle_tail(b, &ctx) }
            })),
        );
    }
    {
        let b = backend.clone();
        bridge.register(
            "tool.terminal.spawn",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let b = b.clone();
                async move { handle_spawn(b, ctx).await }
            })),
        );
    }
    {
        let b = backend.clone();
        bridge.register(
            "tool.terminal.shell.open",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let b = b.clone();
                async move { handle_shell_open(b, ctx).await }
            })),
        );
    }
    {
        let b = backend.clone();
        bridge.register(
            "tool.terminal.shell.input",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let b = b.clone();
                async move { handle_shell_input(b, ctx).await }
            })),
        );
    }
    {
        let b = backend.clone();
        bridge.register(
            "tool.terminal.shell.control",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let b = b.clone();
                async move { handle_shell_control(b, ctx).await }
            })),
        );
    }
    {
        let b = backend;
        bridge.register(
            "tool.terminal.shell.close",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let b = b.clone();
                async move { handle_shell_close(b, &ctx) }
            })),
        );
    }
}

/// PH-TERM-SHELL: `tool.terminal.shell.open` capability —
/// spawns a persistent shell from the operator's
/// `allowed_shells` allowlist with stdin piped. Returns
/// IMMEDIATELY with the session_id; the run continues
/// asynchronously. Operators send bytes via
/// `tool.terminal.shell.input`, read output via
/// `tool.terminal.tail`, and signal EOF via
/// `tool.terminal.shell.close`.
pub fn descriptor_shell_open() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.terminal.shell.open");
    d.major_version = 1;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec![
        "shell:execute".into(),
        "shell:persistent".into(),
        "host:local".into(),
        "destructive:potential".into(),
    ];
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Open a persistent shell session. Request JSON: \
         {command, args, timeout_secs?} where `command` is a bare program \
         name from the operator's `allowed_shells` allowlist. Returns \
         IMMEDIATELY with JSON {session_id, pid, command, timeout_secs, \
         started_at}. Consume via tool.terminal.shell.input + \
         tool.terminal.tail; close stdin with tool.terminal.shell.close; \
         kill outright with tool.terminal.cancel."
            .into(),
    );
    d.categories = vec![
        "mutate".into(),
        "terminal".into(),
        "execute".into(),
        "shell".into(),
        "persistent".into(),
    ];
    d.environment_requirements = vec!["shell:allowlist".into()];
    d.risk_level = RiskLevel::High;
    d
}

/// PH-TERM-SHELL: `tool.terminal.shell.input` capability —
/// writes bytes to a live shell session's stdin. The bytes are
/// taken verbatim (no shell escaping), so callers are
/// responsible for trailing newlines.
pub fn descriptor_shell_input() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.terminal.shell.input");
    d.major_version = 1;
    // Sending the same input twice does send the input twice
    // (shells are stateful) — explicitly not idempotent.
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec![
        "shell:execute".into(),
        "shell:input".into(),
        "destructive:potential".into(),
    ];
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Write bytes to a persistent shell session's stdin. Request JSON: \
         {session_id, bytes?, bytes_base64?}. `bytes` is the UTF-8 happy \
         path (callers are responsible for trailing newlines); \
         `bytes_base64` carries arbitrary binary input. Returns JSON \
         {session_id, written}. INVALID_ARGS when the session is unknown \
         or has been closed."
            .into(),
    );
    d.categories = vec![
        "mutate".into(),
        "terminal".into(),
        "execute".into(),
        "shell".into(),
    ];
    d.environment_requirements = vec!["shell:allowlist".into()];
    d.risk_level = RiskLevel::High;
    d
}

/// PH-TERM-CONTROL: `tool.terminal.shell.control` capability —
/// convenience wrapper that writes a named control byte
/// sequence (etx/eot/tab/cr/lf/enter/esc/backspace/sub/nak) to
/// a session's stdin without the caller having to know the
/// raw byte values or base64-encode them.
///
/// **Honest limitation:** without a PTY, the shell sees these
/// bytes as ordinary stdin input — there is no TTY driver to
/// translate `etx` (0x03) into SIGINT or `eot` (0x04) into
/// EOF. Programs that read stdin as bytes (Python REPL, custom
/// readers) will see the bytes; programs that rely on terminal
/// signal delivery (e.g., `Ctrl+C` interrupting the foreground
/// command in an interactive bash) will not be affected.
pub fn descriptor_shell_control() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.terminal.shell.control");
    d.major_version = 1;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec![
        "shell:execute".into(),
        "shell:input".into(),
        "destructive:potential".into(),
    ];
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Write a named control character sequence to a persistent shell session's \
         stdin. Request JSON: {session_id, control}. Supported control names: \
         etx (ctrl_c), eot (ctrl_d), tab, lf, cr, enter (CRLF on Windows, LF on \
         Unix), esc, backspace (0x7F), bs (0x08), sub (ctrl_z), nak (ctrl_u). \
         Honest limitation: without PTY allocation, these are bytes on the \
         stdin pipe — programs relying on TTY signal delivery (Ctrl+C → SIGINT) \
         will not be affected. Returns {session_id, control, written}."
            .into(),
    );
    d.categories = vec![
        "mutate".into(),
        "terminal".into(),
        "execute".into(),
        "shell".into(),
        "control".into(),
    ];
    d.environment_requirements = vec!["shell:allowlist".into()];
    d.risk_level = RiskLevel::High;
    d
}

/// PH-TERM-SHELL: `tool.terminal.shell.close` capability —
/// drops the stdin writer for a session, sending EOF to the
/// shell process. Most shells exit on EOF; the session
/// continues to be tracked until the child exits (or until
/// timeout / cancel). To kill outright use `tool.terminal.cancel`.
pub fn descriptor_shell_close() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.terminal.shell.close");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["shell:control".into()];
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Close the stdin pipe of a persistent shell session, signalling EOF. \
         Arg is the session id. Returns `ok session=<id>` on hit. The shell \
         process is NOT killed (most shells exit naturally on EOF); to \
         terminate immediately use tool.terminal.cancel."
            .into(),
    );
    d.categories = vec![
        "mutate".into(),
        "terminal".into(),
        "control".into(),
        "shell".into(),
    ];
    d.environment_requirements = vec!["shell:allowlist".into()];
    d.risk_level = RiskLevel::Low;
    d
}

/// PH-TERM-SPAWN: `tool.terminal.spawn` capability — fire-and-
/// forget variant of `tool.terminal.run`. Validates + spawns +
/// registers the session, then returns immediately with the
/// session_id so the caller can poll `tool.terminal.sessions`,
/// `tool.terminal.tail`, and `tool.terminal.audit_recent`. Same
/// allowlist + path-traversal + env / cwd posture as run.
pub fn descriptor_spawn() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.terminal.spawn");
    d.major_version = 1;
    // Spawn is at most once — running the same request twice
    // produces two children.
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec![
        "shell:execute".into(),
        "shell:background".into(),
        "host:local".into(),
        "destructive:potential".into(),
    ];
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Background variant of tool.terminal.run. Request JSON: \
         {command, args, timeout_secs?}. Returns IMMEDIATELY with JSON \
         {session_id, pid, command, timeout_secs, started_at}. The run \
         continues asynchronously; consume it via tool.terminal.tail, \
         tool.terminal.sessions, tool.terminal.audit_recent, or terminate \
         early via tool.terminal.cancel. Same allowlist + cwd + env posture \
         as tool.terminal.run."
            .into(),
    );
    d.categories = vec![
        "mutate".into(),
        "terminal".into(),
        "execute".into(),
        "background".into(),
    ];
    d.environment_requirements = vec!["shell:allowlist".into()];
    d.risk_level = RiskLevel::High;
    d
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
    d.risk_level = RiskLevel::Safe;
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
    d.risk_level = RiskLevel::Low;
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
    d.risk_level = RiskLevel::Safe;
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
    d.risk_level = RiskLevel::Safe;
    d
}

/// PH-TERM-SPAWN: validated + spawned run state. Produced by
/// [`validate_and_spawn`]; consumed by [`drive_to_completion`].
/// Carries everything needed to drive the child to termination
/// AND surface a useful response to the caller, whether the
/// caller is the synchronous `tool.terminal.run` handler, the
/// background `tool.terminal.spawn` handler, or the
/// `tool.terminal.shell.open` handler.
struct SpawnedRun {
    child: tokio::process::Child,
    started: Instant,
    session_id: String,
    cancel_notify: Arc<tokio::sync::Notify>,
    stdout_buf: Arc<Mutex<Vec<u8>>>,
    stderr_buf: Arc<Mutex<Vec<u8>>>,
    stdout_drain: tokio::task::JoinHandle<()>,
    stderr_drain: tokio::task::JoinHandle<()>,
    command: String,
    args: Vec<String>,
    timeout_secs: u64,
    caller_subject_id: String,
    pid: Option<u32>,
}

/// PH-TERM-SHELL: dispatch mode for [`validate_and_spawn`]. `Run`
/// keeps stdin nulled and validates against `allowed_commands`;
/// `Shell` pipes stdin and validates against `allowed_shells`.
/// The shell stdin pipe is taken out of the child and stashed in
/// `backend.shell_stdins` so `tool.terminal.shell.input` can
/// write to it later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpawnMode {
    Run,
    Shell,
}

/// PH-TERM-SPAWN: validate a [`RunRequest`] against the
/// backend's allowlist + posture, configure the tokio
/// [`Command`](tokio::process::Command), spawn the child, take
/// the stdio pipes, register the session, and kick off the
/// stdout/stderr drainer tasks. Pure setup — no wait, no
/// cleanup. Both `tool.terminal.run` and `tool.terminal.spawn`
/// route through this so they share validation + spawn
/// behavior.
async fn validate_and_spawn(
    backend: &Arc<TerminalBackend>,
    ctx: &InvocationCtx,
    req: &RunRequest,
    capability: &'static str,
    mode: SpawnMode,
) -> Result<SpawnedRun, ErrorEnvelope> {
    if req.command.is_empty() {
        return Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!("{capability}: command required"),
            retry_hint: 2,
            retry_after: None,
        });
    }
    if req.command.contains('/') || req.command.contains('\\') {
        return Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!(
                "{capability}: command `{}` contains a path separator; \
                 only bare program names are accepted (operator allowlist \
                 enforces this)",
                req.command
            ),
            retry_hint: 2,
            retry_after: None,
        });
    }
    let (allowlist, allowlist_label) = match mode {
        SpawnMode::Run => (&backend.allowed, "allowed_commands"),
        SpawnMode::Shell => (&backend.allowed_shells, "allowed_shells"),
    };
    if allowlist.is_empty() {
        return Err(ErrorEnvelope {
            kind: error_kinds::POLICY_DENIED,
            cause: format!(
                "{capability}: operator has not configured any entries in \
                 `{allowlist_label}`; the capability fails closed"
            ),
            retry_hint: 0,
            retry_after: None,
        });
    }
    if !allowlist.contains(&req.command) {
        let entries = match mode {
            SpawnMode::Run => backend.cfg.allowed_commands.join(", "),
            SpawnMode::Shell => backend.cfg.allowed_shells.join(", "),
        };
        return Err(ErrorEnvelope {
            kind: error_kinds::POLICY_DENIED,
            cause: format!(
                "{capability}: command `{}` is not in the operator's \
                 `{allowlist_label}` ({entries})",
                req.command,
            ),
            retry_hint: 0,
            retry_after: None,
        });
    }
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
    // PH-TERM-SHELL: Run keeps stdin null (the established
    // safety posture); Shell pipes stdin so the input handler
    // can write to it.
    match mode {
        SpawnMode::Run => {
            command.stdin(std::process::Stdio::null());
        }
        SpawnMode::Shell => {
            command.stdin(std::process::Stdio::piped());
        }
    }
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    command.kill_on_drop(true);

    let mut child = command.spawn().map_err(|e| ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause: format!("{capability}: spawn `{}` failed: {e}", req.command),
        retry_hint: 2,
        retry_after: None,
    })?;
    let stdout_pipe = child
        .stdout
        .take()
        .expect("tool.terminal: stdout pipe present (was piped at spawn)");
    let stderr_pipe = child
        .stderr
        .take()
        .expect("tool.terminal: stderr pipe present (was piped at spawn)");
    // PH-TERM-SHELL: take the stdin pipe out of the Child in
    // shell mode so the input handler can write to it later.
    // Run mode leaves stdin as None (it was Stdio::null at
    // spawn).
    let stdin_pipe = match mode {
        SpawnMode::Shell => Some(
            child
                .stdin
                .take()
                .expect("tool.terminal: stdin pipe present (was piped at spawn)"),
        ),
        SpawnMode::Run => None,
    };

    let session_id = new_session_id();
    let pid = child.id();
    let cancel_notify = Arc::new(tokio::sync::Notify::new());
    let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let caller_subject_id = ctx.caller.subject_id.to_string();
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
                caller_subject_id: caller_subject_id.clone(),
                timeout_secs,
                cancel_notify: cancel_notify.clone(),
                stdout_buf: stdout_buf.clone(),
                stderr_buf: stderr_buf.clone(),
            },
        );
    }
    // PH-TERM-SHELL: stash the stdin pipe in the parallel
    // shell_stdins map keyed by the same session_id. The input
    // handler looks it up here; close drops the entry; the
    // session-cleanup branch of drive_to_completion also
    // removes it so a forgotten close doesn't leak the pipe.
    if let Some(stdin) = stdin_pipe {
        let mut g = backend
            .shell_stdins
            .lock()
            .expect("tool.terminal shell_stdins poisoned");
        g.insert(
            session_id.clone(),
            Arc::new(tokio::sync::Mutex::new(Some(stdin))),
        );
    }

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

    Ok(SpawnedRun {
        child,
        started,
        session_id,
        cancel_notify,
        stdout_buf,
        stderr_buf,
        stdout_drain,
        stderr_drain,
        command: req.command.clone(),
        args: req.args.clone(),
        timeout_secs,
        caller_subject_id,
        pid,
    })
}

/// PH-TERM-SPAWN: drive a [`SpawnedRun`] to termination. Races
/// `child.wait()` against the cancel notify and the hard
/// timeout; joins the drainer tasks; removes the session from
/// the registry; pushes an audit entry; returns the response.
/// Used by both the synchronous run handler (which serializes
/// and returns) and the background spawn handler (which fires
/// this off via `tokio::spawn` and ignores the result).
///
/// Wait-error from `child.wait()` returns `Err(ErrorEnvelope)`
/// WITHOUT pushing audit — wait-error is a harness failure, not
/// a run outcome.
async fn drive_to_completion(
    backend: Arc<TerminalBackend>,
    mut s: SpawnedRun,
) -> Result<RunResponse, ErrorEnvelope> {
    let cancel_fut = s.cancel_notify.notified();
    tokio::pin!(cancel_fut);
    let timeout_fut = tokio::time::sleep(Duration::from_secs(s.timeout_secs));
    tokio::pin!(timeout_fut);
    let outcome = tokio::select! {
        biased;
        res = s.child.wait() => Termination::Exited(res),
        _ = &mut cancel_fut => {
            let _ = s.child.kill().await;
            Termination::Cancelled
        }
        _ = &mut timeout_fut => {
            let _ = s.child.kill().await;
            Termination::TimedOut
        }
    };
    let duration_ms = s.started.elapsed().as_millis() as u64;
    let _ = s.stdout_drain.await;
    let _ = s.stderr_drain.await;
    {
        let mut g = backend
            .sessions
            .lock()
            .expect("tool.terminal sessions poisoned");
        g.remove(&s.session_id);
    }
    // PH-TERM-SHELL: also clear the stdin entry. No-op for
    // non-shell sessions (the map never had an entry for them);
    // for shell sessions that completed without an explicit
    // `tool.terminal.shell.close`, this prevents the writer
    // Arc from leaking past the lifetime of the child.
    {
        let mut g = backend
            .shell_stdins
            .lock()
            .expect("tool.terminal shell_stdins poisoned");
        g.remove(&s.session_id);
    }
    let stdout_bytes = std::mem::take(&mut *s.stdout_buf.lock().expect("stdout buf poisoned"));
    let stderr_bytes = std::mem::take(&mut *s.stderr_buf.lock().expect("stderr buf poisoned"));
    let (stdout, truncated_stdout) = truncate_output(stdout_bytes);
    let (stderr, truncated_stderr) = truncate_output(stderr_bytes);

    let (exit_code, timed_out, cancelled) = match outcome {
        Termination::Exited(Ok(status)) => (status.code(), false, false),
        Termination::Exited(Err(e)) => {
            return Err(ErrorEnvelope {
                kind: error_kinds::RESPONDER_INTERNAL,
                cause: format!("tool.terminal: wait failed: {e}"),
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
        command: s.command.clone(),
        timeout_secs: s.timeout_secs,
    };
    if timed_out {
        tracing::warn!(
            caller = %s.caller_subject_id,
            command = %resp.command,
            timeout_secs = s.timeout_secs,
            duration_ms,
            session_id = %s.session_id,
            "tool.terminal run timed out — child killed"
        );
    } else if cancelled {
        tracing::warn!(
            caller = %s.caller_subject_id,
            command = %resp.command,
            duration_ms,
            session_id = %s.session_id,
            "tool.terminal run cancelled — child killed"
        );
    } else {
        tracing::info!(
            caller = %s.caller_subject_id,
            command = %resp.command,
            exit_code = ?resp.exit_code,
            duration_ms = resp.duration_ms,
            timeout_secs = resp.timeout_secs,
            truncated_stdout = resp.truncated_stdout,
            truncated_stderr = resp.truncated_stderr,
            session_id = %s.session_id,
            "tool.terminal run completed"
        );
    }
    backend.audit.push(TerminalAuditEntry {
        ts_secs: unix_secs(),
        command: resp.command.clone(),
        args: s.args.clone(),
        exit_code: resp.exit_code,
        duration_ms: resp.duration_ms,
        timed_out: resp.timed_out,
        cancelled: resp.cancelled,
        caller_subject_id: s.caller_subject_id.clone(),
    });
    Ok(resp)
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
    let spawned =
        match validate_and_spawn(&backend, &ctx, &req, "tool.terminal.run", SpawnMode::Run).await {
            Ok(s) => s,
            Err(e) => return HandlerOutcome::Err(e),
        };
    match drive_to_completion(backend, spawned).await {
        Ok(resp) => HandlerOutcome::Ok(serde_json::to_vec(&resp).unwrap_or_default()),
        Err(e) => HandlerOutcome::Err(e),
    }
}

/// PH-TERM-SPAWN: wire-shape body for the spawn-only response.
/// Returned immediately by `tool.terminal.spawn` so the caller
/// can start polling tail / sessions / audit with the
/// session_id.
#[derive(Debug, Serialize)]
struct SpawnResponse {
    session_id: String,
    pid: Option<u32>,
    command: String,
    timeout_secs: u64,
    started_at: i64,
}

async fn handle_spawn(backend: Arc<TerminalBackend>, ctx: InvocationCtx) -> HandlerOutcome {
    let req: RunRequest = match serde_json::from_slice(&ctx.args) {
        Ok(r) => r,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("tool.terminal.spawn: bad request shape: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    let spawned =
        match validate_and_spawn(&backend, &ctx, &req, "tool.terminal.spawn", SpawnMode::Run).await
        {
            Ok(s) => s,
            Err(e) => return HandlerOutcome::Err(e),
        };
    let resp = SpawnResponse {
        session_id: spawned.session_id.clone(),
        pid: spawned.pid,
        command: spawned.command.clone(),
        timeout_secs: spawned.timeout_secs,
        started_at: unix_secs(),
    };
    // Kick the run task onto the background runtime and return
    // immediately. The completion path drives audit + cleanup
    // exactly like the synchronous handler; the only difference
    // is that no one is awaiting the RunResponse here. A
    // wait-error from the harness is logged but does not
    // surface to the caller (they already got their session_id).
    let backend_for_task = backend.clone();
    let session_id_for_task = spawned.session_id.clone();
    tokio::spawn(async move {
        if let Err(e) = drive_to_completion(backend_for_task, spawned).await {
            tracing::warn!(
                session_id = %session_id_for_task,
                cause = %e.cause,
                "tool.terminal.spawn: background run completed with wait error"
            );
        }
    });
    HandlerOutcome::Ok(serde_json::to_vec(&resp).unwrap_or_default())
}

/// PH-TERM-SHELL: handle `tool.terminal.shell.open`. Validates
/// against `allowed_shells`, spawns with stdin piped, stashes
/// the stdin writer in `backend.shell_stdins`, and returns
/// immediately with the session id. The run continues
/// asynchronously via `tokio::spawn(drive_to_completion(...))`,
/// matching the spawn handler's posture.
async fn handle_shell_open(backend: Arc<TerminalBackend>, ctx: InvocationCtx) -> HandlerOutcome {
    let req: RunRequest = match serde_json::from_slice(&ctx.args) {
        Ok(r) => r,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("tool.terminal.shell.open: bad request shape: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    let spawned = match validate_and_spawn(
        &backend,
        &ctx,
        &req,
        "tool.terminal.shell.open",
        SpawnMode::Shell,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return HandlerOutcome::Err(e),
    };
    let resp = SpawnResponse {
        session_id: spawned.session_id.clone(),
        pid: spawned.pid,
        command: spawned.command.clone(),
        timeout_secs: spawned.timeout_secs,
        started_at: unix_secs(),
    };
    let backend_for_task = backend.clone();
    let session_id_for_task = spawned.session_id.clone();
    tokio::spawn(async move {
        if let Err(e) = drive_to_completion(backend_for_task, spawned).await {
            tracing::warn!(
                session_id = %session_id_for_task,
                cause = %e.cause,
                "tool.terminal.shell.open: background shell completed with wait error"
            );
        }
    });
    HandlerOutcome::Ok(serde_json::to_vec(&resp).unwrap_or_default())
}

/// PH-TERM-SHELL: handle `tool.terminal.shell.input`. Looks up
/// the session's stdin writer in `backend.shell_stdins`,
/// decodes input bytes (UTF-8 `bytes` or base64 `bytes_base64`),
/// writes + flushes. Returns `{session_id, written}` on
/// success.
/// PH-TERM-SHELL / PH-TERM-CONTROL: shared stdin writer for
/// `tool.terminal.shell.input` and `tool.terminal.shell.control`.
/// Looks the session up in `shell_stdins`, takes the async
/// mutex, writes + flushes. `capability` is threaded into the
/// error envelope so callers see which method failed.
async fn write_to_session_stdin(
    backend: &Arc<TerminalBackend>,
    session_id: &str,
    payload: &[u8],
    capability: &'static str,
) -> Result<usize, ErrorEnvelope> {
    use tokio::io::AsyncWriteExt as _;
    let stdin_arc = {
        let g = backend
            .shell_stdins
            .lock()
            .expect("tool.terminal shell_stdins poisoned");
        match g.get(session_id) {
            Some(arc) => arc.clone(),
            None => {
                return Err(ErrorEnvelope {
                    kind: error_kinds::INVALID_ARGS,
                    cause: format!(
                        "{capability}: session not found or already closed (id='{session_id}')"
                    ),
                    retry_hint: 0,
                    retry_after: None,
                });
            }
        }
    };
    let mut guard = stdin_arc.lock().await;
    let stdin = match guard.as_mut() {
        Some(s) => s,
        None => {
            return Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("{capability}: session stdin has been closed (id='{session_id}')"),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    stdin.write_all(payload).await.map_err(|e| ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause: format!("{capability}: write failed: {e}"),
        retry_hint: 2,
        retry_after: None,
    })?;
    stdin.flush().await.map_err(|e| ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause: format!("{capability}: flush failed: {e}"),
        retry_hint: 2,
        retry_after: None,
    })?;
    Ok(payload.len())
}

async fn handle_shell_input(backend: Arc<TerminalBackend>, ctx: InvocationCtx) -> HandlerOutcome {
    use base64::Engine as _;
    #[derive(Debug, Deserialize)]
    struct ShellInputRequest {
        session_id: String,
        #[serde(default)]
        bytes: String,
        #[serde(default)]
        bytes_base64: String,
    }
    #[derive(Debug, Serialize)]
    struct ShellInputResponse {
        session_id: String,
        written: usize,
    }

    let req: ShellInputRequest = match serde_json::from_slice(&ctx.args) {
        Ok(r) => r,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("tool.terminal.shell.input: bad request shape: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    if req.session_id.is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "tool.terminal.shell.input: session_id required".into(),
            retry_hint: 2,
            retry_after: None,
        });
    }
    let payload: Vec<u8> = if !req.bytes_base64.is_empty() {
        match base64::engine::general_purpose::STANDARD.decode(req.bytes_base64.as_bytes()) {
            Ok(b) => b,
            Err(e) => {
                return HandlerOutcome::Err(ErrorEnvelope {
                    kind: error_kinds::INVALID_ARGS,
                    cause: format!("tool.terminal.shell.input: bad base64: {e}"),
                    retry_hint: 2,
                    retry_after: None,
                });
            }
        }
    } else {
        req.bytes.into_bytes()
    };
    let written = match write_to_session_stdin(
        &backend,
        &req.session_id,
        &payload,
        "tool.terminal.shell.input",
    )
    .await
    {
        Ok(n) => n,
        Err(e) => return HandlerOutcome::Err(e),
    };
    let resp = ShellInputResponse {
        session_id: req.session_id,
        written,
    };
    HandlerOutcome::Ok(serde_json::to_vec(&resp).unwrap_or_default())
}

/// PH-TERM-CONTROL: map a named control sequence to its byte
/// representation. Returns `None` for unknown names. The
/// mapping is platform-aware where it matters: `enter` is
/// CRLF on Windows and LF on Unix to match what a real terminal
/// would feed to the program. The others are protocol-defined
/// single bytes.
fn control_sequence_bytes(name: &str) -> Option<&'static [u8]> {
    // Names are case-insensitive — operators may write either
    // `etx` or `ETX`.
    let lower = name.to_ascii_lowercase();
    let bytes: &'static [u8] = match lower.as_str() {
        // 0x03 — End of Text. Sent by Ctrl+C on most terminals.
        // Without a PTY, shells reading from a pipe see this as
        // an input byte, NOT a SIGINT delivery; document this in
        // the module doc.
        "etx" | "ctrl_c" => b"\x03",
        // 0x04 — End of Transmission. Ctrl+D. On Unix terminals
        // this is the line-buffered EOF marker.
        "eot" | "ctrl_d" => b"\x04",
        // 0x09 — horizontal tab.
        "tab" => b"\x09",
        // 0x0A — line feed (Unix newline). Matches what a real
        // shell sees as "command complete".
        "lf" | "newline" => b"\n",
        // 0x0D — carriage return.
        "cr" => b"\r",
        // Platform-aware enter. CRLF on Windows, LF on Unix.
        "enter" | "return" => {
            if cfg!(windows) {
                b"\r\n"
            } else {
                b"\n"
            }
        }
        // 0x1B — Escape. Beginning of CSI / ANSI sequences.
        "esc" | "escape" => b"\x1b",
        // 0x7F — DEL. Used as backspace by most terminals.
        "backspace" | "del" => b"\x7f",
        // 0x08 — BS. Some terminals use this for backspace
        // instead of 0x7F; expose it explicitly so operators
        // can pick.
        "bs" | "backspace_bs" => b"\x08",
        // 0x1A — Substitute. Ctrl+Z. Job-control suspend on
        // Unix terminals (no effect without a PTY).
        "sub" | "ctrl_z" => b"\x1a",
        // 0x15 — NAK. Ctrl+U. Line-kill on Unix terminals.
        "nak" | "ctrl_u" => b"\x15",
        _ => return None,
    };
    Some(bytes)
}

/// PH-TERM-CONTROL: handle `tool.terminal.shell.control`.
/// Looks up the named control sequence and writes its byte(s)
/// to the session stdin via the shared writer.
async fn handle_shell_control(backend: Arc<TerminalBackend>, ctx: InvocationCtx) -> HandlerOutcome {
    #[derive(Debug, Deserialize)]
    struct ShellControlRequest {
        session_id: String,
        control: String,
    }
    #[derive(Debug, Serialize)]
    struct ShellControlResponse {
        session_id: String,
        control: String,
        written: usize,
    }
    let req: ShellControlRequest = match serde_json::from_slice(&ctx.args) {
        Ok(r) => r,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("tool.terminal.shell.control: bad request shape: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    if req.session_id.is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "tool.terminal.shell.control: session_id required".into(),
            retry_hint: 2,
            retry_after: None,
        });
    }
    let bytes = match control_sequence_bytes(&req.control) {
        Some(b) => b,
        None => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!(
                    "tool.terminal.shell.control: unknown control '{}'; supported: \
                     etx (ctrl_c), eot (ctrl_d), tab, lf, cr, enter, esc, backspace, \
                     bs, sub (ctrl_z), nak (ctrl_u)",
                    req.control
                ),
                retry_hint: 0,
                retry_after: None,
            });
        }
    };
    let written = match write_to_session_stdin(
        &backend,
        &req.session_id,
        bytes,
        "tool.terminal.shell.control",
    )
    .await
    {
        Ok(n) => n,
        Err(e) => return HandlerOutcome::Err(e),
    };
    let resp = ShellControlResponse {
        session_id: req.session_id,
        control: req.control,
        written,
    };
    HandlerOutcome::Ok(serde_json::to_vec(&resp).unwrap_or_default())
}

/// PH-TERM-SHELL: handle `tool.terminal.shell.close`. Drops the
/// stdin pipe from `backend.shell_stdins`, which closes the OS
/// pipe and signals EOF to the shell. The child is NOT killed
/// — most shells exit naturally on EOF; operators wanting an
/// immediate kill use `tool.terminal.cancel`.
fn handle_shell_close(backend: Arc<TerminalBackend>, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("tool.terminal.shell.close arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    if s.is_empty() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: "tool.terminal.shell.close: session_id required".into(),
            retry_hint: 2,
            retry_after: None,
        });
    }
    let removed = {
        let mut g = backend
            .shell_stdins
            .lock()
            .expect("tool.terminal shell_stdins poisoned");
        g.remove(s).is_some()
    };
    if removed {
        HandlerOutcome::Ok(format!("ok session={s}\n").into_bytes())
    } else {
        HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!(
                "tool.terminal.shell.close: session not found or stdin already closed (id='{s}')"
            ),
            retry_hint: 0,
            retry_after: None,
        })
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
            allowed_shells: vec![],
            max_timeout_secs: 30,
            inherit_env: false,
            working_dir: None,
        }
    }

    fn cfg_with_shells(allowed: &[&str], shells: &[&str]) -> TerminalConfig {
        TerminalConfig {
            allowed_commands: allowed.iter().map(|s| s.to_string()).collect(),
            allowed_shells: shells.iter().map(|s| s.to_string()).collect(),
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

    // ── PH-TERM-SPAWN: tool.terminal.spawn ─────────────────────────

    /// Cross-platform always-present binary for the live-spawn
    /// integration test below. On Windows `cmd.exe` is in
    /// System32; on Unix `sh` is in /bin via the OS PATH.
    #[cfg(windows)]
    const REAL_SPAWN_BIN: &str = "cmd";
    #[cfg(unix)]
    const REAL_SPAWN_BIN: &str = "sh";

    #[cfg(windows)]
    fn real_spawn_quick_exit_args() -> &'static [&'static str] {
        &["/c", "exit"]
    }
    #[cfg(unix)]
    fn real_spawn_quick_exit_args() -> &'static [&'static str] {
        &["-c", "exit 0"]
    }

    #[test]
    fn spawn_descriptor_shape() {
        let d = descriptor_spawn();
        assert_eq!(d.method_name, "tool.terminal.spawn");
        assert_eq!(d.major_version, 1);
        assert!(matches!(d.idempotency, Idempotency::AtMostOnce));
        assert!(matches!(d.cost_class, CostClass::Cheap));
        assert!(d.sensitivity_tags.iter().any(|t| t == "shell:execute"));
        assert!(d.sensitivity_tags.iter().any(|t| t == "shell:background"));
        assert!(
            d.sensitivity_tags
                .iter()
                .any(|t| t == "destructive:potential")
        );
        assert!(d.requires_groups.iter().any(|g| g == "operators"));
        assert!(d.categories.iter().any(|c| c == "background"));
    }

    #[tokio::test]
    async fn spawn_bad_json_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let ctx = ctx_with_args(b"not-json");
        match handle_spawn(b, ctx).await {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("bad request shape"));
                assert_eq!(e.kind, error_kinds::INVALID_ARGS);
            }
            _ => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn spawn_empty_command_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let arg = br#"{"command":"","args":[]}"#;
        match handle_spawn(b, ctx_with_args(arg)).await {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("command required"));
                assert_eq!(e.kind, error_kinds::INVALID_ARGS);
            }
            _ => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn spawn_path_separator_in_command_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let arg = br#"{"command":"bin/echo","args":[]}"#;
        match handle_spawn(b, ctx_with_args(arg)).await {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("path separator"));
                assert_eq!(e.kind, error_kinds::INVALID_ARGS);
            }
            _ => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn spawn_disallowed_command_rejected_policy_denied() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let arg = br#"{"command":"rm","args":["-rf","/"]}"#;
        match handle_spawn(b, ctx_with_args(arg)).await {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("allowed_commands"));
                assert_eq!(e.kind, error_kinds::POLICY_DENIED);
            }
            _ => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn spawn_real_subprocess_returns_session_immediately() {
        // Real-spawn integration test. Uses cmd.exe on Windows
        // / sh on Unix, both of which are always present.
        let b = Arc::new(TerminalBackend::new(cfg(&[REAL_SPAWN_BIN])).unwrap());
        let args_json = serde_json::to_string(real_spawn_quick_exit_args()).unwrap();
        let body =
            format!(r#"{{"command":"{REAL_SPAWN_BIN}","args":{args_json},"timeout_secs":5}}"#);
        let ctx = ctx_with_args(body.as_bytes());

        let started = Instant::now();
        let result = handle_spawn(b.clone(), ctx).await;
        // Spawn must return well under the timeout (which is
        // 5 seconds) — typically tens of milliseconds.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "spawn should return immediately, took {:?}",
            started.elapsed()
        );

        let body = match result {
            HandlerOutcome::Ok(b) => b,
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["session_id"].as_str().is_some());
        assert!(v["pid"].as_u64().is_some());
        assert_eq!(v["command"], REAL_SPAWN_BIN);
        assert_eq!(v["timeout_secs"], 5);

        // Give the background task time to complete the
        // quick-exit and push the audit entry.
        for _ in 0..50 {
            if !b.audit_snapshot(10).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let snap = b.audit_snapshot(10);
        assert_eq!(
            snap.len(),
            1,
            "audit entry should land after background completion"
        );
        assert_eq!(snap[0].command, REAL_SPAWN_BIN);
        assert!(!snap[0].timed_out);
        assert!(!snap[0].cancelled);
        // exit 0 → exit_code Some(0) on both platforms.
        assert_eq!(snap[0].exit_code, Some(0));

        // Session should have been removed from the live registry
        // by the completion path.
        assert!(b.snapshot_sessions().is_empty());
    }

    // ── PH-TERM-SHELL: tool.terminal.shell.{open,input,close} ─────

    #[test]
    fn shell_descriptors_shape() {
        let open = descriptor_shell_open();
        assert_eq!(open.method_name, "tool.terminal.shell.open");
        assert!(
            open.sensitivity_tags
                .iter()
                .any(|t| t == "shell:persistent")
        );
        assert!(open.categories.iter().any(|c| c == "persistent"));

        let input = descriptor_shell_input();
        assert_eq!(input.method_name, "tool.terminal.shell.input");
        assert!(input.sensitivity_tags.iter().any(|t| t == "shell:input"));

        let close = descriptor_shell_close();
        assert_eq!(close.method_name, "tool.terminal.shell.close");
        assert!(matches!(close.idempotency, Idempotency::Idempotent));
    }

    #[test]
    fn backend_rejects_shell_allowlist_path_separator() {
        let err = TerminalBackend::new(cfg_with_shells(&["echo"], &["bin/sh"])).unwrap_err();
        assert!(err.contains("path separator"));
    }

    #[test]
    fn backend_rejects_empty_shell_allowlist_entry() {
        let err = TerminalBackend::new(cfg_with_shells(&["echo"], &[""])).unwrap_err();
        assert!(err.contains("empty entry"));
    }

    #[tokio::test]
    async fn shell_open_fails_closed_when_allowlist_empty() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let arg = br#"{"command":"sh","args":[]}"#;
        match handle_shell_open(b, ctx_with_args(arg)).await {
            HandlerOutcome::Err(e) => {
                assert_eq!(e.kind, error_kinds::POLICY_DENIED);
                assert!(e.cause.contains("`allowed_shells`"));
            }
            _ => panic!("expected POLICY_DENIED"),
        }
    }

    #[tokio::test]
    async fn shell_open_disallowed_command_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg_with_shells(&["echo"], &["sh"])).unwrap());
        let arg = br#"{"command":"bash","args":[]}"#;
        match handle_shell_open(b, ctx_with_args(arg)).await {
            HandlerOutcome::Err(e) => {
                assert_eq!(e.kind, error_kinds::POLICY_DENIED);
                assert!(e.cause.contains("allowed_shells"));
            }
            _ => panic!("expected POLICY_DENIED"),
        }
    }

    #[tokio::test]
    async fn shell_input_unknown_session_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let arg = br#"{"session_id":"abc123","bytes":"ls\n"}"#;
        match handle_shell_input(b, ctx_with_args(arg)).await {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("session not found or already closed"));
                assert_eq!(e.kind, error_kinds::INVALID_ARGS);
            }
            _ => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn shell_input_bad_base64_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let arg = br#"{"session_id":"abc123","bytes_base64":"!!!not-base64!!!"}"#;
        match handle_shell_input(b, ctx_with_args(arg)).await {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("bad base64"));
                assert_eq!(e.kind, error_kinds::INVALID_ARGS);
            }
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn shell_close_unknown_session_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let ctx = ctx_with_args(b"abc123");
        match handle_shell_close(b, &ctx) {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("session not found"));
                assert_eq!(e.kind, error_kinds::INVALID_ARGS);
            }
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn shell_close_empty_arg_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let ctx = ctx_with_args(b"");
        match handle_shell_close(b, &ctx) {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("session_id required")),
            _ => panic!("expected Err"),
        }
    }

    /// Cross-platform shell binary for the live-spawn shell test.
    #[cfg(windows)]
    const REAL_SHELL_BIN: &str = "cmd";
    #[cfg(unix)]
    const REAL_SHELL_BIN: &str = "sh";

    #[tokio::test]
    async fn shell_open_real_subprocess_full_lifecycle() {
        // End-to-end: open a real shell, send input that
        // produces stdout, close stdin, observe the audit
        // entry.
        let b = Arc::new(TerminalBackend::new(cfg_with_shells(&[], &[REAL_SHELL_BIN])).unwrap());
        let req = format!(r#"{{"command":"{REAL_SHELL_BIN}","args":[],"timeout_secs":10}}"#);
        let open_body = match handle_shell_open(b.clone(), ctx_with_args(req.as_bytes())).await {
            HandlerOutcome::Ok(body) => body,
            HandlerOutcome::Err(e) => panic!("open failed: {}", e.cause),
        };
        let open_v: serde_json::Value = serde_json::from_slice(&open_body).unwrap();
        let session_id = open_v["session_id"].as_str().unwrap().to_string();
        assert!(!session_id.is_empty());
        assert!(open_v["pid"].as_u64().is_some());

        // Send a command that produces deterministic output and
        // then exits. On Windows `cmd` exits on `exit\r\n`; on
        // Unix `sh` exits on EOF or `exit\n`.
        let echo_cmd = if cfg!(windows) {
            "echo relix-shell-marker\r\nexit\r\n"
        } else {
            "echo relix-shell-marker\nexit\n"
        };
        let input_req = serde_json::to_string(&serde_json::json!({
            "session_id": session_id,
            "bytes": echo_cmd,
        }))
        .unwrap();
        let input_body =
            match handle_shell_input(b.clone(), ctx_with_args(input_req.as_bytes())).await {
                HandlerOutcome::Ok(b) => b,
                HandlerOutcome::Err(e) => panic!("input failed: {}", e.cause),
            };
        let input_v: serde_json::Value = serde_json::from_slice(&input_body).unwrap();
        assert_eq!(input_v["session_id"], session_id);
        assert_eq!(input_v["written"], echo_cmd.len());

        // Give the shell time to consume input, produce output,
        // and exit. The audit ring is the most reliable signal
        // that the background task has finished cleanup.
        for _ in 0..100 {
            if !b.audit_snapshot(10).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let snap = b.audit_snapshot(10);
        assert_eq!(snap.len(), 1, "audit entry should land after shell exit");
        assert_eq!(snap[0].command, REAL_SHELL_BIN);
        assert!(!snap[0].timed_out);
        assert!(!snap[0].cancelled);

        // Live registry should be empty + stdin map should be
        // empty (cleanup branch in drive_to_completion removes
        // both).
        assert!(b.snapshot_sessions().is_empty());
        assert!(b.shell_stdins.lock().unwrap().is_empty());
    }

    // ── PH-TERM-CONTROL: tool.terminal.shell.control ──────────────

    #[test]
    fn shell_control_descriptor_shape() {
        let d = descriptor_shell_control();
        assert_eq!(d.method_name, "tool.terminal.shell.control");
        assert!(matches!(d.idempotency, Idempotency::AtMostOnce));
        assert!(d.sensitivity_tags.iter().any(|t| t == "shell:input"));
        assert!(d.categories.iter().any(|c| c == "control"));
    }

    #[test]
    fn control_sequence_bytes_named_singletons() {
        assert_eq!(control_sequence_bytes("etx"), Some(&b"\x03"[..]));
        assert_eq!(control_sequence_bytes("ctrl_c"), Some(&b"\x03"[..]));
        assert_eq!(control_sequence_bytes("eot"), Some(&b"\x04"[..]));
        assert_eq!(control_sequence_bytes("ctrl_d"), Some(&b"\x04"[..]));
        assert_eq!(control_sequence_bytes("tab"), Some(&b"\x09"[..]));
        assert_eq!(control_sequence_bytes("lf"), Some(&b"\n"[..]));
        assert_eq!(control_sequence_bytes("cr"), Some(&b"\r"[..]));
        assert_eq!(control_sequence_bytes("esc"), Some(&b"\x1b"[..]));
        assert_eq!(control_sequence_bytes("escape"), Some(&b"\x1b"[..]));
        assert_eq!(control_sequence_bytes("backspace"), Some(&b"\x7f"[..]));
        assert_eq!(control_sequence_bytes("del"), Some(&b"\x7f"[..]));
        assert_eq!(control_sequence_bytes("bs"), Some(&b"\x08"[..]));
        assert_eq!(control_sequence_bytes("sub"), Some(&b"\x1a"[..]));
        assert_eq!(control_sequence_bytes("ctrl_z"), Some(&b"\x1a"[..]));
        assert_eq!(control_sequence_bytes("nak"), Some(&b"\x15"[..]));
        assert_eq!(control_sequence_bytes("ctrl_u"), Some(&b"\x15"[..]));
    }

    #[test]
    fn control_sequence_bytes_is_case_insensitive() {
        assert_eq!(control_sequence_bytes("ETX"), Some(&b"\x03"[..]));
        assert_eq!(control_sequence_bytes("Ctrl_C"), Some(&b"\x03"[..]));
        assert_eq!(control_sequence_bytes("Escape"), Some(&b"\x1b"[..]));
    }

    #[test]
    fn control_sequence_bytes_unknown_returns_none() {
        assert!(control_sequence_bytes("not-a-control").is_none());
        assert!(control_sequence_bytes("").is_none());
        // Aliases that aren't in the mapping table.
        assert!(control_sequence_bytes("ctrl_a").is_none());
    }

    #[test]
    fn control_sequence_bytes_enter_is_platform_aware() {
        let bytes = control_sequence_bytes("enter").unwrap();
        if cfg!(windows) {
            assert_eq!(bytes, b"\r\n");
        } else {
            assert_eq!(bytes, b"\n");
        }
    }

    #[tokio::test]
    async fn shell_control_bad_json_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let ctx = ctx_with_args(b"not-json");
        match handle_shell_control(b, ctx).await {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("bad request shape"));
                assert_eq!(e.kind, error_kinds::INVALID_ARGS);
            }
            _ => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn shell_control_empty_session_id_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let arg = br#"{"session_id":"","control":"etx"}"#;
        match handle_shell_control(b, ctx_with_args(arg)).await {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("session_id required"));
            }
            _ => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn shell_control_unknown_control_rejected_with_supported_list() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let arg = br#"{"session_id":"abc123","control":"f13"}"#;
        match handle_shell_control(b, ctx_with_args(arg)).await {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("unknown control"));
                // The error lists the supported names so operators
                // know what they can use.
                assert!(e.cause.contains("etx"));
                assert!(e.cause.contains("enter"));
            }
            _ => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn shell_control_unknown_session_rejected() {
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        // Control is valid; session_id does not exist.
        let arg = br#"{"session_id":"missing","control":"etx"}"#;
        match handle_shell_control(b, ctx_with_args(arg)).await {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("session not found or already closed"));
                assert!(e.cause.contains("tool.terminal.shell.control"));
            }
            _ => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn shell_control_unknown_session_includes_no_match_for_control() {
        // Regression guard: the error message should not look
        // like the validation failed at the control mapping
        // step (it should report session-not-found, since the
        // control name IS valid).
        let b = Arc::new(TerminalBackend::new(cfg(&["echo"])).unwrap());
        let arg = br#"{"session_id":"missing","control":"etx"}"#;
        match handle_shell_control(b, ctx_with_args(arg)).await {
            HandlerOutcome::Err(e) => {
                assert!(!e.cause.contains("unknown control"));
            }
            _ => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn shell_close_drops_stdin_entry_without_killing_session() {
        // Manually wire up a session record + a stdin entry so
        // we can test close in isolation (without spawning a
        // real shell).
        let b = Arc::new(TerminalBackend::new(cfg_with_shells(&["echo"], &["sh"])).unwrap());
        let session_id = "manual-shell-session";

        // Insert a session record.
        {
            let mut g = b.sessions.lock().unwrap();
            g.insert(
                session_id.into(),
                mk_session_record(session_id, unix_secs(), "sh"),
            );
        }
        // Insert a stdin entry containing None (the actual
        // ChildStdin requires a real spawn — we just need the
        // map slot to exercise the close path's remove logic).
        {
            let mut g = b.shell_stdins.lock().unwrap();
            g.insert(session_id.into(), Arc::new(tokio::sync::Mutex::new(None)));
        }
        assert!(b.shell_stdins.lock().unwrap().contains_key(session_id));

        let ctx = ctx_with_args(session_id.as_bytes());
        match handle_shell_close(b.clone(), &ctx) {
            HandlerOutcome::Ok(body) => {
                let s = String::from_utf8(body).unwrap();
                assert!(s.contains(&format!("ok session={session_id}")));
            }
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        }

        // stdin entry gone; session record still present (close
        // does NOT kill the process, only signals EOF).
        assert!(!b.shell_stdins.lock().unwrap().contains_key(session_id));
        assert!(b.sessions.lock().unwrap().contains_key(session_id));
    }

    /// PH-RISK-PIN-ALL: pin the risk tier of every shipped
    /// terminal descriptor. Observation surfaces (sessions /
    /// audit_recent / tail) are Safe. Cooperative control
    /// (cancel / shell.close — internal-only state changes)
    /// is Low. Execution surfaces (spawn + the four shell.*
    /// caps that drive a subprocess) are High. terminal.run
    /// itself lives in tool/mod.rs and is pinned by the test
    /// in that module.
    #[test]
    fn terminal_descriptors_have_explicit_non_unknown_risk() {
        let pinned: &[(&str, CapabilityDescriptor, RiskLevel)] = &[
            (
                "tool.terminal.sessions",
                descriptor_sessions(),
                RiskLevel::Safe,
            ),
            (
                "tool.terminal.audit_recent",
                descriptor_audit_recent(),
                RiskLevel::Safe,
            ),
            ("tool.terminal.tail", descriptor_tail(), RiskLevel::Safe),
            ("tool.terminal.cancel", descriptor_cancel(), RiskLevel::Low),
            (
                "tool.terminal.shell.close",
                descriptor_shell_close(),
                RiskLevel::Low,
            ),
            ("tool.terminal.spawn", descriptor_spawn(), RiskLevel::High),
            (
                "tool.terminal.shell.open",
                descriptor_shell_open(),
                RiskLevel::High,
            ),
            (
                "tool.terminal.shell.input",
                descriptor_shell_input(),
                RiskLevel::High,
            ),
            (
                "tool.terminal.shell.control",
                descriptor_shell_control(),
                RiskLevel::High,
            ),
        ];
        for (name, d, expected) in pinned {
            assert_ne!(
                d.risk_level,
                RiskLevel::Unknown,
                "{name} defaulted to Unknown risk"
            );
            assert_eq!(
                d.risk_level, *expected,
                "{name} risk tier drifted (expected {expected:?})"
            );
        }
    }
}
