//! The **Rig** layer — Relix's universal agent-backend contract
//! (the "plug in any agent" foundation; see
//! `docs/relix-agent-adapters.md`).
//!
//! A **Rig** is *what powers an Operative* — the swappable backend
//! that actually runs a Brief. The rest of Relix (the Brief ledger,
//! the heartbeat loop, governance) never cares *which* Rig an
//! Operative uses: it hands the Rig a [`RigRunRequest`] and gets a
//! [`RigOutcome`] back. Adding support for a new agent product —
//! an embedded Hermes, a Claude / Codex CLI on a subscription, a
//! remote API agent — is implementing this one trait and
//! registering it.
//!
//! **Governance scales with the Rig, the sandbox is always the
//! floor.** A *rich* Rig (a plugged-in Hermes, ACP) lets Relix gate
//! each tool call from inside; a *thin* Rig (a headless CLI, a
//! generic process) can only be governed at the box wall plus the
//! bridge-back token. Each Rig declares which it is via
//! [`Rig::governance`] so the dispatcher can size the sandbox
//! accordingly.
//!
//! This module is the contract + registry + a built-in reference
//! adapter (`echo`). Real Rigs live behind the same trait.

use std::collections::BTreeMap;
use std::sync::Arc;

pub mod bridge;

/// A request to run a Brief on a Rig — what the dispatcher hands an
/// agent backend when it wakes an Operative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RigRunRequest {
    /// The Brief (coordinator task id) being worked.
    pub brief_id: String,
    /// The Operative (agent id) assigned to it.
    pub agent_id: String,
    /// The Guild (tenant) the work belongs to.
    pub tenant_id: String,
    /// The work to do — the Brief's description / instruction
    /// bundle, assembled by the dispatcher.
    pub prompt: String,
    /// Opaque additional context (goal ancestry, prior-run summary,
    /// linked Dossiers, …). The Rig passes it through to the agent.
    pub context: String,
    /// PILLAR 2 (bridge-back): the scoped per-run token the agent
    /// uses to call Relix's API back (comment, sub-brief, request a
    /// Clearance). Empty when no bridge is configured. A Rig injects
    /// it into the agent's environment at run time.
    pub bridge_token: String,
    /// Optional per-run working directory override. When set it wins
    /// over the Rig's configured `working_dir`. Validated (must exist +
    /// be a directory) before spawn.
    pub working_dir: Option<std::path::PathBuf>,
}

impl RigRunRequest {
    pub fn new(
        brief_id: impl Into<String>,
        agent_id: impl Into<String>,
        tenant_id: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            brief_id: brief_id.into(),
            agent_id: agent_id.into(),
            tenant_id: tenant_id.into(),
            prompt: prompt.into(),
            context: String::new(),
            bridge_token: String::new(),
            working_dir: None,
        }
    }

    /// Attach opaque context (builder style).
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = context.into();
        self
    }

    /// Attach a bridge-back token (builder style).
    pub fn with_bridge_token(mut self, token: impl Into<String>) -> Self {
        self.bridge_token = token.into();
        self
    }

    /// Pin the working directory for this run (builder style).
    pub fn with_working_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }
}

/// The outcome of a Rig run, reported back to the dispatcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RigOutcome {
    /// The run finished and produced a result summary. The
    /// dispatcher records the Shift and may move the Brief toward
    /// `in_review` / `done`.
    Done { summary: String },
    /// The run did useful work but the Brief needs another Shift
    /// later (a durable yield / continuation). The dispatcher
    /// releases the Claim and the Brief stays workable.
    Continue { note: String },
    /// The run failed. `retryable` lets the dispatcher distinguish a
    /// transient failure (retry next tick) from a hard one (escalate
    /// to the Desk).
    Failed { reason: String, retryable: bool },
}

/// How much Relix can govern *inside* a Rig. Rich Rigs expose every
/// tool call for gating; thin Rigs can only be bounded by their
/// sandbox + the scoped bridge-back token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RigGovernance {
    /// Per-tool-call gating available inside the Rig (Hermes, ACP).
    PerToolCall,
    /// Only box-level (sandbox) governance — the floor (headless
    /// CLIs, generic processes). The dispatcher gives these tighter
    /// sandboxes.
    BoxLevel,
}

impl RigGovernance {
    /// Stable wire string for manifests / the agent-config UI.
    pub fn as_str(&self) -> &'static str {
        match self {
            RigGovernance::PerToolCall => "per_tool_call",
            RigGovernance::BoxLevel => "box_level",
        }
    }
}

/// A registry-level description of one Rig — what the Keys /
/// agent-config UI needs to let an operator pick a backend, without
/// reaching into the trait object.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RigInfo {
    pub name: String,
    pub display_name: String,
    /// `per_tool_call` or `box_level` — how deeply Relix governs it.
    pub governance: String,
    pub bridge_back: bool,
    pub structured_output: bool,
    pub billing: RigBilling,
    pub probe: RigProbe,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RigBilling {
    pub mode: String,
    pub provider: Option<String>,
    pub subscription_included: bool,
    pub quota_window: Option<String>,
}

impl RigBilling {
    pub fn metered(provider: impl Into<String>) -> Self {
        Self {
            mode: "metered".to_string(),
            provider: Some(provider.into()),
            subscription_included: false,
            quota_window: None,
        }
    }

    pub fn subscription(provider: impl Into<String>, quota_window: impl Into<String>) -> Self {
        Self {
            mode: "subscription".to_string(),
            provider: Some(provider.into()),
            subscription_included: true,
            quota_window: Some(quota_window.into()),
        }
    }

    pub fn none() -> Self {
        Self {
            mode: "none".to_string(),
            provider: None,
            subscription_included: false,
            quota_window: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RigProbe {
    pub status: String,
    pub detail: String,
    pub install_hint: Option<String>,
}

impl RigProbe {
    pub fn available(detail: impl Into<String>) -> Self {
        Self {
            status: "available".to_string(),
            detail: detail.into(),
            install_hint: None,
        }
    }

    pub fn missing(detail: impl Into<String>, install_hint: Option<String>) -> Self {
        Self {
            status: "missing".to_string(),
            detail: detail.into(),
            install_hint,
        }
    }

    /// Build a probe with an explicit structured status. The CLI rigs use
    /// this to report the richer readiness vocabulary (`missing_binary` /
    /// `not_authenticated` / `unsupported_version` / `interactive_only` /
    /// `probe_failed`) — anything other than `available` reads as "not
    /// runnable" by the dispatcher + dashboard.
    pub fn with_status(
        status: impl Into<String>,
        detail: impl Into<String>,
        install_hint: Option<String>,
    ) -> Self {
        Self {
            status: status.into(),
            detail: detail.into(),
            install_hint,
        }
    }

    /// True only when the adapter is actually runnable right now.
    pub fn is_available(&self) -> bool {
        self.status == "available"
    }
}

/// A real, noninteractive readiness check for a CLI adapter. Running the
/// `probe_args` (e.g. `--version`) against the binary distinguishes
/// "installed and runs" from "needs login", "wants a TTY", or "broken".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessCheck {
    /// Args for the cheap, noninteractive readiness command (no auth, no
    /// billable call) — typically `--version`.
    pub probe_args: Vec<String>,
    /// What to tell the operator when auth is the blocker.
    pub login_hint: String,
    /// Optional SECOND, auth-verifying command (e.g. `auth status --text`).
    /// When set, `available` additionally requires this command to report
    /// a logged-in session — so an installed-but-logged-out CLI resolves
    /// to `not_authenticated` instead of a misleading `available`. The
    /// command must itself be noninteractive (text output, no prompt).
    pub auth_args: Option<Vec<String>>,
}

/// Outcome of running a readiness command — the raw signals the
/// classifier turns into a structured status. Separated so the
/// classification logic is a pure, unit-testable function.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadinessSignals {
    /// The binary was not found on PATH (no command ran).
    pub missing_binary: bool,
    /// The command did not return within the probe timeout.
    pub timed_out: bool,
    /// The OS failed to spawn it (other than not-found).
    pub spawn_error: Option<String>,
    /// Process exited 0.
    pub exit_ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Classify a readiness probe's raw signals into one of the structured
/// statuses. Pure + keyword-driven so it is unit-testable with mocked
/// command outputs. Returns `(status, detail)`.
pub fn classify_readiness(sig: &ReadinessSignals) -> (&'static str, String) {
    if sig.missing_binary {
        return ("missing_binary", "binary not found on PATH".to_string());
    }
    if let Some(e) = &sig.spawn_error {
        return ("probe_failed", format!("could not spawn: {e}"));
    }
    if sig.timed_out {
        return (
            "interactive_only",
            "the CLI did not return to a noninteractive probe — it likely \
             requires a TTY / interactive prompt and cannot run headless"
                .to_string(),
        );
    }
    let blob = format!("{}\n{}", sig.stdout, sig.stderr).to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| blob.contains(n));
    // Auth keywords win even on a zero exit (some CLIs print a login
    // nudge to stderr without failing).
    if has(&[
        "not authenticated",
        "not logged in",
        "please log in",
        "please login",
        "run `claude login`",
        "run claude login",
        "run `codex login`",
        "run codex login",
        "you are not signed in",
        "sign in",
        "unauthorized",
        "401",
        "authentication required",
        "no credentials",
        "login required",
    ]) {
        return (
            "not_authenticated",
            "the CLI is installed but not logged in".to_string(),
        );
    }
    if sig.exit_ok {
        let line = sig
            .stdout
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        return (
            "available",
            if line.is_empty() {
                "installed and runs noninteractively".to_string()
            } else {
                format!("installed: {line}")
            },
        );
    }
    // Non-zero exit that wasn't an auth error.
    if has(&[
        "unknown flag",
        "unrecognized",
        "no such subcommand",
        "invalid option",
        "unexpected argument",
        "unknown option",
    ]) {
        return (
            "unsupported_version",
            "the CLI rejected the probe flags — its version may be \
             incompatible with this adapter"
                .to_string(),
        );
    }
    let detail = if !sig.stderr.trim().is_empty() {
        sig.stderr.trim()
    } else {
        sig.stdout.trim()
    };
    (
        "probe_failed",
        format!("readiness probe failed: {detail}"),
    )
}

/// Auth-status output that proves the CLI is **logged out**. Checked
/// first because several of these contain a "logged in" substring
/// (e.g. "you are not signed in" ⊃ "signed in").
const AUTH_LOGGED_OUT: &[&str] = &[
    "not logged in",
    "not authenticated",
    "unauthenticated",
    "logged out",
    "not signed in",
    "please log in",
    "please login",
    "no credentials",
    "login required",
    "you are not",
    "run `claude auth login`",
    "run claude auth login",
    "run `claude login`",
    "401",
];

/// Auth-status output that proves the CLI is **logged in**.
const AUTH_LOGGED_IN: &[&str] = &[
    "logged in",
    "authenticated",
    "signed in",
    "account",
    "subscription",
    "claude max",
    "credentials found",
    "active account",
    "api key",
];

/// Classify readiness from a `--version` probe PLUS an optional
/// auth-status probe. The version probe decides install/runs; only when
/// the binary clearly runs do we consult auth. This is what makes a
/// logged-in CLI `available` and an installed-but-logged-out CLI
/// `not_authenticated` (instead of a misleading `available`). Pure +
/// keyword-driven so it is unit-testable with mocked outputs.
pub fn classify_readiness_with_auth(
    version: &ReadinessSignals,
    auth: Option<&ReadinessSignals>,
) -> (&'static str, String) {
    let (vstatus, vdetail) = classify_readiness(version);
    // If the binary itself isn't cleanly runnable, the version verdict
    // (missing / interactive_only / unsupported / probe_failed /
    // not_authenticated-from-version) stands — auth is moot.
    if vstatus != "available" {
        return (vstatus, vdetail);
    }
    let Some(auth) = auth else {
        return (vstatus, vdetail);
    };
    // The binary runs; interpret the auth-status command.
    if auth.timed_out {
        return (
            "interactive_only",
            "the auth-status check did not return — the CLI likely needs \
             an interactive session and cannot confirm login headless"
                .to_string(),
        );
    }
    if auth.missing_binary || auth.spawn_error.is_some() {
        // The binary ran for --version but the auth subcommand couldn't
        // start (e.g. an older CLI without `auth status`). Don't claim
        // logged-in, but don't block a clearly-installed binary either.
        return ("available", format!("{vdetail}; auth status unavailable"));
    }
    let blob = format!("{}\n{}", auth.stdout, auth.stderr).to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| blob.contains(n));
    // Logged-out FIRST (its phrases can contain a logged-in substring).
    if has(AUTH_LOGGED_OUT) {
        return (
            "not_authenticated",
            "installed but not logged in".to_string(),
        );
    }
    if has(AUTH_LOGGED_IN) {
        let line = auth
            .stdout
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim();
        return (
            "available",
            if line.is_empty() {
                format!("{vdetail}; logged in")
            } else {
                format!("{vdetail}; {line}")
            },
        );
    }
    // The binary runs but the auth output is unrecognized — don't regress
    // a working install on an unfamiliar status format; report available
    // and say so honestly.
    ("available", format!("{vdetail}; auth status unrecognized"))
}

/// A **Rig** — a pluggable agent backend. The uniform contract
/// behind every Operative's "what powers it."
pub trait Rig: Send + Sync {
    /// The Rig's stable type name (e.g. `echo`, `hermes`, `claude`,
    /// `codex`). Used as its registry key.
    fn name(&self) -> &str;

    /// A human label for the Rig. Defaults to [`Rig::name`].
    fn display_name(&self) -> &str {
        self.name()
    }

    /// How deeply Relix can govern inside this Rig. Defaults to the
    /// conservative `BoxLevel` (thin) — a Rig opts *up* to
    /// `PerToolCall` only when it genuinely exposes its tools.
    fn governance(&self) -> RigGovernance {
        RigGovernance::BoxLevel
    }

    fn supports_bridge_back(&self) -> bool {
        true
    }

    fn structured_output(&self) -> bool {
        false
    }

    fn billing(&self) -> RigBilling {
        RigBilling::none()
    }

    fn probe(&self) -> RigProbe {
        RigProbe::available("no probe required")
    }

    /// Run one Brief and report the outcome. Synchronous by
    /// contract; async backends (process spawn, HTTP) run their I/O
    /// and block the worker thread (the dispatcher calls this off
    /// the async runtime).
    fn run(&self, req: &RigRunRequest) -> RigOutcome;
}

/// A registry of Rigs, keyed by [`Rig::name`]. Built-ins are
/// registered at startup; operator / third-party Rigs register the
/// same way, so "plug in any agent" is open-ended. Last writer wins
/// (an operator Rig may override a built-in of the same name).
#[derive(Clone, Default)]
pub struct RigRegistry {
    rigs: BTreeMap<String, Arc<dyn Rig>>,
    /// The Guild-default Rig name, used when an Operative has no Rig
    /// of its own. `None` = no default (unconfigured agents don't
    /// dispatch).
    default_name: Option<String>,
}

impl RigRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry pre-loaded with the built-in Rigs.
    pub fn with_builtins() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(EchoRig));
        r
    }

    /// Register a Rig under its [`Rig::name`]. Overrides any
    /// existing Rig of the same name.
    pub fn register(&mut self, rig: Arc<dyn Rig>) {
        self.rigs.insert(rig.name().to_string(), rig);
    }

    /// Set the Guild-default Rig name (builder style). An Operative
    /// with no Rig of its own resolves to this one.
    pub fn with_default(mut self, name: impl Into<String>) -> Self {
        self.default_name = Some(name.into());
        self
    }

    /// Set / clear the Guild-default Rig name.
    pub fn set_default(&mut self, name: Option<String>) {
        self.default_name = name;
    }

    /// The configured default Rig name, if any.
    pub fn default_name(&self) -> Option<&str> {
        self.default_name.as_deref()
    }

    /// Look up a Rig by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Rig>> {
        self.rigs.get(name).cloned()
    }

    /// Resolve the Rig to run for an Operative: its `preferred` Rig
    /// if set and known, else the Guild default. `None` when neither
    /// resolves — the Brief is left for the Desk. This is the
    /// dispatcher's single resolution point.
    pub fn resolve(&self, preferred: Option<&str>) -> Option<Arc<dyn Rig>> {
        if let Some(name) = preferred.filter(|s| !s.is_empty())
            && let Some(rig) = self.get(name)
        {
            return Some(rig);
        }
        self.default_name.as_deref().and_then(|d| self.get(d))
    }

    /// All registered Rig names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.rigs.keys().cloned().collect()
    }

    /// Describe every registered Rig (name + label + governance),
    /// sorted by name — the structured feed for the agent-config UI.
    pub fn describe(&self) -> Vec<RigInfo> {
        self.rigs
            .values()
            .map(|r| RigInfo {
                name: r.name().to_string(),
                display_name: r.display_name().to_string(),
                governance: r.governance().as_str().to_string(),
                bridge_back: r.supports_bridge_back(),
                structured_output: r.structured_output(),
                billing: r.billing(),
                probe: r.probe(),
            })
            .collect()
    }

    /// How many Rigs are registered.
    pub fn len(&self) -> usize {
        self.rigs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rigs.is_empty()
    }
}

/// The built-in **`echo`** Rig — the contract's canonical minimal
/// adapter. It "runs" a Brief by echoing the prompt back as the
/// result. Used in tests and as the reference any real Rig (Hermes,
/// Claude, Codex, remote) is modelled on. Thin by governance: it
/// does no tool calls, so there is nothing inside to gate.
pub struct EchoRig;

impl Rig for EchoRig {
    fn name(&self) -> &str {
        "echo"
    }

    fn display_name(&self) -> &str {
        "Echo (built-in reference)"
    }

    fn supports_bridge_back(&self) -> bool {
        false
    }

    fn run(&self, req: &RigRunRequest) -> RigOutcome {
        if req.prompt.trim().is_empty() {
            RigOutcome::Failed {
                reason: "empty prompt".to_string(),
                retryable: false,
            }
        } else {
            RigOutcome::Done {
                summary: format!("echo: {}", req.prompt.trim()),
            }
        }
    }
}

/// A **process** Rig — runs an Operative by spawning an external
/// command. This is the generic backend behind the CLI Rigs (a
/// Claude / Codex / Gemini CLI on a subscription) and any
/// `process`-style agent: the Brief's prompt is piped to the
/// child's stdin and the child's stdout becomes the result. A
/// non-zero exit, or a spawn/wait failure, is a *retryable*
/// [`RigOutcome::Failed`].
///
/// Thin by governance: Relix can't see the child's internal tool
/// calls, so a process Rig must run inside a Relix-governed sandbox
/// — the box is the boundary.
///
/// NOTE: the prompt is written to stdin synchronously before stdout
/// is drained, which is fine for the modest prompts/outputs of the
/// dispatch path. Streaming large I/O on separate threads is a
/// future refinement the real CLI adapters will layer on.
/// How a process Rig's captured stdout is turned into a [`RigOutcome`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RigOutputFormat {
    /// Treat stdout verbatim as the result summary (the generic default).
    #[default]
    Raw,
    /// Parse Claude Code's `--output-format stream-json` JSONL: extract
    /// the terminal `type:"result"` event's `result` text as the summary,
    /// map `is_error` to a failure, and surface `permission_denials`
    /// (Relix runs Claude noninteractively, so tool approvals are NOT
    /// auto-granted — file/command actions are blocked + reported).
    ClaudeStreamJson,
}

/// The fields Relix reads from Claude Code's terminal `result` event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaudeRunResult {
    /// The model's final answer (`result.result`).
    pub text: String,
    /// `result.is_error` — Claude's own success/failure verdict.
    pub is_error: bool,
    /// `result.subtype` (e.g. `success`, `error_max_turns`,
    /// `error_during_execution`).
    pub subtype: String,
    /// Count of `result.permission_denials` — tools Claude wanted to run
    /// but couldn't (Relix grants no interactive approval).
    pub permission_denials: usize,
    /// `result.num_turns` (agentic turns taken).
    pub num_turns: i64,
}

/// Parse Claude Code `stream-json` (JSONL) stdout and return the terminal
/// `result` event's fields. Scans for the LAST `{"type":"result",…}`
/// line (the authoritative terminal event), ignoring the `system` /
/// `assistant` / hook noise. Returns `None` when no result event is
/// present (an interrupted / malformed run), so the caller falls back to
/// exit-code handling. Pure + line-driven → unit-testable with mocked
/// JSONL.
pub fn parse_claude_stream_json(stdout: &str) -> Option<ClaudeRunResult> {
    let mut found: Option<ClaudeRunResult> = None;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("result") {
            continue;
        }
        found = Some(ClaudeRunResult {
            text: v
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("")
                .to_string(),
            is_error: v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false),
            subtype: v
                .get("subtype")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            permission_denials: v
                .get("permission_denials")
                .and_then(|d| d.as_array())
                .map(|a| a.len())
                .unwrap_or(0),
            num_turns: v.get("num_turns").and_then(|n| n.as_i64()).unwrap_or(0),
        });
    }
    found
}

pub struct ProcessRig {
    name: String,
    program: String,
    args: Vec<String>,
    /// Cap on the child's captured stdout (the result summary), so a
    /// runaway CLI can't flood the dispatch path / context.
    max_output_bytes: usize,
    /// How stdout is interpreted into a [`RigOutcome`] (verbatim, or a
    /// Claude `stream-json` parse).
    output_format: RigOutputFormat,
    /// How deeply Relix governs this specific adapter. Defaults to
    /// the conservative `BoxLevel` (a plain stdio process is a black
    /// box). An operator opts *up* to `PerToolCall` only when their
    /// adapter genuinely surfaces tool calls Relix can gate (e.g. it
    /// speaks the Macro `@relix-call` protocol or ACP).
    governance: RigGovernance,
    structured_output: bool,
    billing: RigBilling,
    install_hint: Option<String>,
    /// Hard wall-clock cap on a single run. On expiry the child is
    /// killed (cancellation) and the run reports a retryable timeout.
    timeout: std::time::Duration,
    /// Working directory the child runs in. `None` inherits the
    /// coordinator's CWD; `Some(dir)` is validated (must be an existing
    /// directory) before spawn. The per-run request can override this.
    working_dir: Option<std::path::PathBuf>,
    /// Optional noninteractive readiness check (CLI adapters). When set,
    /// `probe()` actually RUNS the readiness command and classifies the
    /// result (installed / needs-login / wants-TTY / broken) instead of
    /// only checking PATH.
    readiness: Option<ReadinessCheck>,
    /// Extra absolute executable candidates tried when `PATH` resolution
    /// finds no directly-spawnable `.exe` (e.g. Claude's npm-installed
    /// real `claude.exe` deep under `node_modules`, which isn't on
    /// `PATH`). See [`resolve_program`].
    fallback_paths: Vec<std::path::PathBuf>,
}

/// How long a readiness probe command may run before it's treated as
/// `interactive_only` (it hung waiting for a TTY).
pub const READINESS_PROBE_TIMEOUT_SECS: u64 = 8;

/// Default stdout cap for a process Rig — generous enough for a real
/// agent's final answer, bounded enough to stop a firehose.
pub const DEFAULT_RIG_MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Default hard timeout for a single process-Rig run (10 minutes). A
/// real coding agent can take minutes; anything past this is a runaway
/// and gets killed.
pub const DEFAULT_RIG_TIMEOUT_SECS: u64 = 600;

impl ProcessRig {
    pub fn new(name: impl Into<String>, program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            args,
            max_output_bytes: DEFAULT_RIG_MAX_OUTPUT_BYTES,
            governance: RigGovernance::BoxLevel,
            structured_output: false,
            billing: RigBilling::none(),
            install_hint: None,
            timeout: std::time::Duration::from_secs(DEFAULT_RIG_TIMEOUT_SECS),
            working_dir: None,
            readiness: None,
            fallback_paths: Vec::new(),
            output_format: RigOutputFormat::Raw,
        }
    }

    /// Choose how the child's stdout becomes a [`RigOutcome`] (verbatim,
    /// or a Claude `stream-json` parse). Builder style.
    pub fn with_output_format(mut self, fmt: RigOutputFormat) -> Self {
        self.output_format = fmt;
        self
    }

    /// Configure a noninteractive readiness probe (CLI adapters). The
    /// `probe_args` (e.g. `["--version"]`) must be cheap, auth-free, and
    /// noninteractive; `login_hint` is shown when auth is the blocker.
    pub fn with_readiness(mut self, probe_args: Vec<String>, login_hint: impl Into<String>) -> Self {
        self.readiness = Some(ReadinessCheck {
            probe_args,
            login_hint: login_hint.into(),
            auth_args: None,
        });
        self
    }

    /// Add a SECOND, auth-verifying readiness command (e.g.
    /// `["auth", "status", "--text"]`) on top of [`Self::with_readiness`].
    /// With it set, `available` requires both `--version` to run AND this
    /// command to report a logged-in session — so an installed-but-
    /// logged-out CLI resolves to `not_authenticated`. No-op if
    /// `with_readiness` wasn't called first.
    pub fn with_auth_probe(mut self, auth_args: Vec<String>) -> Self {
        if let Some(r) = self.readiness.as_mut() {
            r.auth_args = Some(auth_args);
        }
        self
    }

    /// Add an absolute executable fallback path tried when `PATH` yields
    /// no directly-spawnable `.exe` (Windows npm-shim resilience).
    pub fn with_fallback_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.fallback_paths.push(path.into());
        self
    }

    /// Override the hard run timeout. Clamped to at least 1 second.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout.max(std::time::Duration::from_secs(1));
        self
    }

    /// Pin the working directory the child runs in (builder style).
    pub fn with_working_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// The configured run timeout.
    pub fn timeout(&self) -> std::time::Duration {
        self.timeout
    }

    /// Cap the captured stdout to `n` bytes (truncated on a char
    /// boundary). Clamped to at least 1.
    pub fn with_max_output_bytes(mut self, n: usize) -> Self {
        self.max_output_bytes = n.max(1);
        self
    }

    /// Declare how deeply Relix governs this adapter. Only set
    /// `PerToolCall` when the process genuinely exposes its tool
    /// calls for gating; the default `BoxLevel` is the safe floor.
    pub fn with_governance(mut self, governance: RigGovernance) -> Self {
        self.governance = governance;
        self
    }

    pub fn with_structured_output(mut self, structured_output: bool) -> Self {
        self.structured_output = structured_output;
        self
    }

    pub fn with_billing(mut self, billing: RigBilling) -> Self {
        self.billing = billing;
        self
    }

    pub fn with_install_hint(mut self, install_hint: impl Into<String>) -> Self {
        self.install_hint = Some(install_hint.into());
        self
    }

    /// The program this Rig spawns.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The arguments passed to the program.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// The current stdout cap.
    pub fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }
}

impl Rig for ProcessRig {
    fn name(&self) -> &str {
        &self.name
    }

    fn governance(&self) -> RigGovernance {
        self.governance
    }

    fn structured_output(&self) -> bool {
        self.structured_output
    }

    fn billing(&self) -> RigBilling {
        self.billing.clone()
    }

    fn probe(&self) -> RigProbe {
        // Resolve the program the SAME way `run` spawns it — honoring
        // PATH+PATHEXT and the npm-shim fallback — so the probe can never
        // report a binary the runner can't actually launch (the old bug:
        // a `claude.cmd` shim "existed" but couldn't be spawned directly,
        // so the probe lied `probe_failed`).
        let resolved = resolve_program(&self.program, &self.fallback_paths);
        // Without a readiness check this is a plain process Rig — a
        // resolution check is all we can honestly assert.
        let Some(readiness) = &self.readiness else {
            return if resolved.is_some() {
                RigProbe::available(format!("{} found", self.program))
            } else {
                RigProbe::with_status(
                    "missing_binary",
                    format!("{} not found on PATH", self.program),
                    self.install_hint.clone(),
                )
            };
        };
        // A CLI adapter: run the noninteractive readiness command(s)
        // against the RESOLVED spawnable and classify the result.
        let Some(spawn) = resolved else {
            return RigProbe::with_status(
                "missing_binary",
                format!("{} not found on PATH", self.program),
                self.install_hint.clone(),
            );
        };
        let timeout = std::time::Duration::from_secs(READINESS_PROBE_TIMEOUT_SECS);
        let version = run_readiness_probe_spawnable(&spawn, &readiness.probe_args, timeout);
        let auth = readiness
            .auth_args
            .as_ref()
            .map(|a| run_readiness_probe_spawnable(&spawn, a, timeout));
        let (status, detail) = classify_readiness_with_auth(&version, auth.as_ref());
        // Pick the most actionable hint for the resolved status.
        let hint = match status {
            "not_authenticated" => Some(readiness.login_hint.clone()),
            "available" => None,
            _ => self.install_hint.clone(),
        };
        RigProbe::with_status(status, detail, hint)
    }

    fn run(&self, req: &RigRunRequest) -> RigOutcome {
        use std::io::{Read, Write};
        use std::process::Stdio;

        // Resolve + validate the working directory. A per-run override
        // wins over the Rig default. A configured-but-missing directory
        // is a hard (non-retryable) failure — never silently fall back
        // to the coordinator's CWD.
        let working_dir = req.working_dir.as_ref().or(self.working_dir.as_ref());
        if let Some(dir) = working_dir
            && !dir.is_dir()
        {
            return RigOutcome::Failed {
                reason: format!("working dir does not exist: {}", dir.display()),
                retryable: false,
            };
        }

        // Resolve the program to a spawnable (PATH+PATHEXT, npm-shim
        // fallback, `.cmd`/`.bat` → `cmd.exe /C`). A non-resolvable
        // program is a clear, non-retryable failure (it isn't installed).
        let Some(spawn) = resolve_program(&self.program, &self.fallback_paths) else {
            return RigOutcome::Failed {
                reason: format!("{} not found on PATH", self.program),
                retryable: false,
            };
        };
        let mut command = command_for(&spawn, &self.args);
        command
            // The agent learns its own scope from the environment;
            // the bridge token (when present) is how it calls Relix
            // back, scoped to exactly this Brief + Operative.
            .env("RELIX_BRIEF_ID", &req.brief_id)
            .env("RELIX_AGENT_ID", &req.agent_id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = working_dir {
            command.current_dir(dir);
        }
        if !req.bridge_token.is_empty() {
            command.env("RELIX_BRIDGE_TOKEN", &req.bridge_token);
        }
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                return RigOutcome::Failed {
                    reason: format!("spawn {}: {e}", self.program),
                    retryable: true,
                };
            }
        };

        // Drain stdout/stderr on dedicated threads into shared buffers so
        // a chatty child cannot deadlock by filling a pipe buffer while we
        // wait. Buffers are read incrementally so a timeout can snapshot
        // partial output WITHOUT joining the readers (a killed child's
        // grandchild can keep the pipe open — joining would hang).
        use std::sync::{Arc, Mutex};
        let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let reader = |pipe: Option<std::process::ChildStdout>, buf: Arc<Mutex<Vec<u8>>>| {
            pipe.map(|mut p| {
                std::thread::spawn(move || {
                    let mut tmp = [0u8; 8192];
                    loop {
                        match p.read(&mut tmp) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if let Ok(mut b) = buf.lock() {
                                    b.extend_from_slice(&tmp[..n]);
                                }
                            }
                        }
                    }
                })
            })
        };
        let out_handle = reader(child.stdout.take(), stdout_buf.clone());
        let err_handle = child.stderr.take().map(|mut p| {
            let buf = stderr_buf.clone();
            std::thread::spawn(move || {
                let mut tmp = [0u8; 8192];
                loop {
                    match p.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut b) = buf.lock() {
                                b.extend_from_slice(&tmp[..n]);
                            }
                        }
                    }
                }
            })
        });

        // Pipe the prompt to the child, then close stdin (EOF).
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(req.prompt.as_bytes());
        }

        // Wait with a hard deadline. On expiry, KILL the child
        // (cancellation) and report a retryable timeout.
        let deadline = std::time::Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break None; // timed out
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => {
                    let _ = child.kill();
                    return RigOutcome::Failed {
                        reason: format!("wait {}: {e}", self.program),
                        retryable: true,
                    };
                }
            }
        };

        // Give the readers a brief grace to flush, then snapshot whatever
        // they have — NEVER join unboundedly (a timed-out grandchild may
        // hold the pipe open). Unfinished reader threads are detached and
        // exit on their own when the pipe finally closes.
        let grace = std::time::Instant::now() + std::time::Duration::from_millis(500);
        for h in [out_handle, err_handle].into_iter().flatten() {
            while !h.is_finished() && std::time::Instant::now() < grace {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let out_bytes = stdout_buf.lock().map(|b| b.clone()).unwrap_or_default();
        let err_bytes = stderr_buf.lock().map(|b| b.clone()).unwrap_or_default();
        let raw_stdout = String::from_utf8_lossy(&out_bytes).trim().to_string();
        let raw_stderr = String::from_utf8_lossy(&err_bytes).trim().to_string();
        // Redact obvious secrets BEFORE anything is persisted/returned.
        let stdout = self.cap(redact_secrets(&raw_stdout, &req.bridge_token));
        let stderr = self.cap(redact_secrets(&raw_stderr, &req.bridge_token));

        match status {
            None => RigOutcome::Failed {
                reason: format!(
                    "timed out after {}s (killed)",
                    self.timeout.as_secs()
                ),
                retryable: true,
            },
            Some(status) => {
                // Claude's terminal `result` event is authoritative — it
                // can exit 0 with `is_error`, or non-zero while still
                // carrying a usable result — so parse it FIRST (off the
                // raw, pre-redaction stdout so the JSON stays valid).
                if matches!(self.output_format, RigOutputFormat::ClaudeStreamJson)
                    && let Some(outcome) = self.claude_outcome(&raw_stdout, &req.bridge_token)
                {
                    return outcome;
                }
                if status.success() {
                    RigOutcome::Done { summary: stdout }
                } else {
                    let code = status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".to_string());
                    let detail = if stderr.is_empty() { stdout } else { stderr };
                    RigOutcome::Failed {
                        reason: format!("exit {code}: {detail}"),
                        retryable: true,
                    }
                }
            }
        }
    }
}

impl ProcessRig {
    /// Map Claude Code `stream-json` stdout to a [`RigOutcome`]. Parses
    /// the terminal `result` event (off the raw, pre-redaction stdout so
    /// the JSON parses), then redacts + caps only the extracted answer:
    /// - `is_error` → a clear, non-retryable failure (`subtype` + text).
    /// - otherwise → `Done` with the model's answer; when one or more
    ///   tool permissions were denied (Relix runs Claude noninteractively,
    ///   so file/command tool use is NOT auto-approved) the summary leads
    ///   with an unmissable `⚠ N tool permission(s) denied` caveat so a
    ///   blocked action is never mistaken for a completed one.
    ///
    /// Returns `None` when no terminal `result` event is present, so the
    /// caller falls back to exit-code handling (the run was interrupted /
    /// malformed and must report truthfully).
    fn claude_outcome(&self, raw_stdout: &str, bridge_token: &str) -> Option<RigOutcome> {
        let parsed = parse_claude_stream_json(raw_stdout)?;
        let text = self.cap(redact_secrets(parsed.text.trim(), bridge_token));
        if parsed.is_error {
            let reason = if text.is_empty() {
                format!("claude run failed ({})", parsed.subtype)
            } else {
                format!("claude {}: {text}", parsed.subtype)
            };
            return Some(RigOutcome::Failed {
                reason,
                retryable: false,
            });
        }
        let summary = if parsed.permission_denials > 0 {
            format!(
                "⚠ {} tool permission(s) denied — Relix runs Claude noninteractively and does \
                 not auto-approve tool use, so file/command actions were blocked. Model reply: {}",
                parsed.permission_denials,
                if text.is_empty() { "(no text)" } else { &text }
            )
        } else if text.is_empty() {
            "claude completed with no text output".to_string()
        } else {
            text
        };
        Some(RigOutcome::Done { summary })
    }

    /// Truncate captured output to `max_output_bytes` on a char boundary.
    fn cap(&self, mut s: String) -> String {
        if s.len() > self.max_output_bytes {
            let mut end = self.max_output_bytes;
            while end > 0 && !s.is_char_boundary(end) {
                end -= 1;
            }
            s.truncate(end);
        }
        s
    }
}

/// Run a CLI adapter's noninteractive readiness command and collect the
/// raw [`ReadinessSignals`]. Stdin is closed (`null`) so a CLI that reads
/// stdin gets immediate EOF instead of hanging; stdout/stderr are
/// captured with a hard timeout (a hang → `timed_out`, classified as
/// `interactive_only`). Safe argv only — no shell.
pub fn run_readiness_probe(
    program: &str,
    args: &[String],
    timeout: std::time::Duration,
) -> ReadinessSignals {
    // Resolve the same way `run` spawns (PATH+PATHEXT, `.cmd` → cmd.exe).
    // No npm-shim fallbacks here — callers that need them pass a resolved
    // [`Spawnable`] to [`run_readiness_probe_spawnable`].
    match resolve_program(program, &[]) {
        Some(spawn) => run_readiness_probe_spawnable(&spawn, args, timeout),
        None => ReadinessSignals {
            missing_binary: true,
            ..Default::default()
        },
    }
}

/// Run a readiness command against an already-resolved [`Spawnable`]
/// (handles the `.cmd`/`.bat` → `cmd.exe /C` wrapping) and collect the
/// raw [`ReadinessSignals`]. Stdin is closed (`null`); stdout/stderr are
/// captured with a hard timeout (a hang → `timed_out`). Safe argv only.
pub fn run_readiness_probe_spawnable(
    spawn: &Spawnable,
    args: &[String],
    timeout: std::time::Duration,
) -> ReadinessSignals {
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};

    let mut child = match command_for(spawn, args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return ReadinessSignals {
                spawn_error: Some(e.to_string()),
                ..Default::default()
            };
        }
    };
    let out_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let err_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let drain = |pipe: Option<std::process::ChildStdout>, buf: Arc<Mutex<Vec<u8>>>| {
        pipe.map(|mut p| {
            std::thread::spawn(move || {
                let mut tmp = [0u8; 4096];
                while let Ok(n) = p.read(&mut tmp) {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut b) = buf.lock() {
                        b.extend_from_slice(&tmp[..n]);
                    }
                }
            })
        })
    };
    let oh = drain(child.stdout.take(), out_buf.clone());
    let eh = child.stderr.take().map(|mut p| {
        let buf = err_buf.clone();
        std::thread::spawn(move || {
            let mut tmp = [0u8; 4096];
            while let Ok(n) = p.read(&mut tmp) {
                if n == 0 {
                    break;
                }
                if let Ok(mut b) = buf.lock() {
                    b.extend_from_slice(&tmp[..n]);
                }
            }
        })
    });
    let deadline = std::time::Instant::now() + timeout;
    let (timed_out, exit_ok) = loop {
        match child.try_wait() {
            Ok(Some(s)) => break (false, s.success()),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break (true, false);
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            Err(_) => {
                let _ = child.kill();
                break (false, false);
            }
        }
    };
    let grace = std::time::Instant::now() + std::time::Duration::from_millis(300);
    for h in [oh, eh].into_iter().flatten() {
        while !h.is_finished() && std::time::Instant::now() < grace {
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    }
    let stdout = String::from_utf8_lossy(&out_buf.lock().map(|b| b.clone()).unwrap_or_default())
        .trim()
        .to_string();
    let stderr = String::from_utf8_lossy(&err_buf.lock().map(|b| b.clone()).unwrap_or_default())
        .trim()
        .to_string();
    ReadinessSignals {
        missing_binary: false,
        timed_out,
        spawn_error: None,
        exit_ok,
        stdout,
        stderr,
    }
}

/// Redact obvious secrets from captured agent output before it is
/// persisted to the Chronicle / returned to the dashboard. Heuristic but
/// deliberately conservative: it never leaks the per-run bridge-back
/// token and masks common API-key / token shapes.
///
/// - The literal `bridge_token` value (when non-empty) → `***`.
/// - Tokens with well-known prefixes (`sk-`, `ghp_`, `gho_`, `xox`,
///   `AKIA`, …) → `***`.
/// - Any standalone high-entropy run of ≥ 40 hex/base64url chars → `***`.
/// - `NAME_(KEY|TOKEN|SECRET|PASSWORD)=value` → keeps the name, masks
///   the value.
pub fn redact_secrets(text: &str, bridge_token: &str) -> String {
    let mut pre = text.to_string();
    if bridge_token.len() >= 8 {
        pre = pre.replace(bridge_token, "***");
    }
    const PREFIXES: &[&str] = &["sk-", "ghp_", "gho_", "ghu_", "ghs_", "xox", "AKIA", "AIza"];
    fn looks_secret(tok: &str) -> bool {
        if PREFIXES.iter().any(|p| tok.starts_with(p)) && tok.len() >= 16 {
            return true;
        }
        tok.len() >= 40
            && tok
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && tok.chars().any(|c| c.is_ascii_digit())
    }
    fn mask_word(word: &str) -> String {
        // `NAME_(KEY|TOKEN|SECRET|PASSWORD)=value` → mask only the value.
        if let Some((name, val)) = word.split_once('=') {
            let up = name.to_ascii_uppercase();
            if (up.contains("KEY") || up.contains("TOKEN") || up.contains("SECRET") || up.contains("PASSWORD"))
                && val.len() >= 6
            {
                return format!("{name}=***");
            }
        }
        if looks_secret(word) {
            "***".to_string()
        } else {
            word.to_string()
        }
    }
    // Walk the text emitting separators verbatim so newlines / tabs /
    // multiple spaces (i.e. the agent's formatting) survive; only the
    // word runs are inspected + possibly masked.
    let is_word = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '=' | '.' | '/' | '+');
    let mut out = String::with_capacity(pre.len());
    let mut word = String::new();
    for c in pre.chars() {
        if is_word(c) {
            word.push(c);
        } else {
            if !word.is_empty() {
                out.push_str(&mask_word(&word));
                word.clear();
            }
            out.push(c);
        }
    }
    if !word.is_empty() {
        out.push_str(&mask_word(&word));
    }
    out
}

// ── CLI subscription Rigs ─────────────────────────────────
//
/// A resolved, spawnable program — *how* to invoke a CLI adapter, not
/// just *whether* it exists. The distinction matters on Windows, where
/// `claude` is an npm shim (`claude.cmd`) that `CreateProcess` (Rust's
/// `Command`) cannot spawn directly: it must run through `cmd.exe /C`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Spawnable {
    /// A directly-spawnable executable — an `.exe`/`.com` (or an
    /// extensionless binary) on Windows, or any resolved binary on Unix.
    /// Spawned via `Command::new(path)`.
    Direct(std::path::PathBuf),
    /// A Windows batch shim (`.cmd`/`.bat`). `CreateProcess` can't run
    /// these directly, so it is spawned via `cmd.exe /C <shim> <args…>`
    /// with each arg passed as a DISCRETE argv element (never a joined
    /// shell string), so a Brief's content can't inject a command.
    BatchShim(std::path::PathBuf),
}

/// The Windows executable extensions this resolver understands, in
/// preference order (direct-spawnable first). Read from `PATHEXT` when
/// set, falling back to the conventional default. Lowercased, deduped to
/// the four we actually support.
fn windows_exec_exts() -> Vec<String> {
    let raw = std::env::var("PATHEXT")
        .or_else(|_| std::env::var("Pathext"))
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let mut out: Vec<String> = Vec::new();
    for e in raw.split(';') {
        let e = e.trim().to_ascii_lowercase();
        if matches!(e.as_str(), ".exe" | ".com" | ".bat" | ".cmd") && !out.contains(&e) {
            out.push(e);
        }
    }
    if out.is_empty() {
        out = vec![".com".into(), ".exe".into(), ".bat".into(), ".cmd".into()];
    }
    out
}

/// Classify an existing file path into a [`Spawnable`], or `None` if it
/// isn't a file we know how to run. On Windows `.exe`/`.com` → `Direct`,
/// `.cmd`/`.bat` → `BatchShim`, and **any other extension (including
/// none) → `None`** — Windows `CreateProcess` cannot run an extensionless
/// or non-PE file, so an npm `claude` *sh* shim (a 300-byte script that
/// shares the PATH dir with `claude.cmd`) must NOT be treated as a direct
/// executable (doing so spawns it and fails `os error 193`). On Unix any
/// existing file → `Direct` (executability is enforced by the OS at
/// spawn; Unix binaries carry no extension).
fn classify_file(path: &std::path::Path) -> Option<Spawnable> {
    if !path.is_file() {
        return None;
    }
    if !cfg!(windows) {
        return Some(Spawnable::Direct(path.to_path_buf()));
    }
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("exe") | Some("com") => Some(Spawnable::Direct(path.to_path_buf())),
        Some("cmd") | Some("bat") => Some(Spawnable::BatchShim(path.to_path_buf())),
        _ => None,
    }
}

/// Resolve a program name to a [`Spawnable`], honoring `PATH` + `PATHEXT`
/// on Windows and **preferring a directly-spawnable `.exe`/`.com`** over
/// a `.cmd`/`.bat` shim. `fallback_paths` are extra absolute candidates
/// tried when `PATH` yields no direct executable (e.g. Claude's
/// npm-installed real `claude.exe` deep under `node_modules`, which is
/// not itself on `PATH`). Resolution order: (1) a `Direct` exe found on
/// `PATH`, (2) a `Direct` exe from `fallback_paths`, (3) a `BatchShim`
/// found on `PATH`, (4) any `fallback_paths` candidate (even a shim).
///
/// An explicit path (one containing a separator) is resolved as-is (with
/// `PATHEXT` appended when it has no extension) and never falls back to
/// `PATH`.
pub fn resolve_program(program: &str, fallback_paths: &[std::path::PathBuf]) -> Option<Spawnable> {
    use std::path::Path;
    let p = Path::new(program);
    let has_sep = program.contains('/') || program.contains('\\');
    if has_sep || p.is_absolute() {
        if let Some(s) = classify_file(p) {
            return Some(s);
        }
        if cfg!(windows) && p.extension().is_none() {
            for ext in windows_exec_exts() {
                let cand = std::path::PathBuf::from(format!("{program}{ext}"));
                if let Some(s) = classify_file(&cand) {
                    return Some(s);
                }
            }
        }
        return None;
    }
    // Bare name: scan PATH dirs with the PATHEXT extension set.
    let dirs: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|pe| std::env::split_paths(&pe).collect())
        .unwrap_or_default();
    resolve_in_dirs(program, &dirs, &path_search_exts(), fallback_paths)
}

/// Extension set probed for a bare name, in preference order. On Windows
/// `""` (an already-suffixed/extensionless match) plus the `PATHEXT`
/// entries; on Unix just `""`.
fn path_search_exts() -> Vec<String> {
    if cfg!(windows) {
        let mut v = vec![String::new()];
        v.extend(windows_exec_exts());
        v
    } else {
        vec![String::new()]
    }
}

/// Core of [`resolve_program`] for a bare name, with the search dirs +
/// extensions injected (so it is unit-testable without mutating the
/// process-global `PATH`). Prefers a `Direct` exe (found anywhere in the
/// search path, regardless of dir/PATHEXT order) over a `.cmd`/`.bat`
/// shim, then a `Direct` fallback exe, then a PATH shim, then any
/// fallback.
fn resolve_in_dirs(
    program: &str,
    dirs: &[std::path::PathBuf],
    exts: &[String],
    fallback_paths: &[std::path::PathBuf],
) -> Option<Spawnable> {
    let mut shim: Option<Spawnable> = None;
    for dir in dirs {
        for ext in exts {
            let cand = dir.join(format!("{program}{ext}"));
            match classify_file(&cand) {
                Some(s @ Spawnable::Direct(_)) => return Some(s),
                // Remember the first shim, but keep scanning for a real exe.
                Some(s @ Spawnable::BatchShim(_)) if shim.is_none() => shim = Some(s),
                _ => {}
            }
        }
    }
    // No direct exe on PATH — prefer a fallback REAL exe over a PATH shim.
    for fp in fallback_paths {
        if let Some(s @ Spawnable::Direct(_)) = classify_file(fp) {
            return Some(s);
        }
    }
    if let Some(s) = shim {
        return Some(s);
    }
    for fp in fallback_paths {
        if let Some(s) = classify_file(fp) {
            return Some(s);
        }
    }
    None
}

/// The trusted `cmd.exe` to wrap batch shims with — the one under
/// `%SystemRoot%\System32`, never a `cmd` picked up from a hijacked
/// `PATH`. Falls back to the bare name only if `SystemRoot` is unset.
fn trusted_cmd_exe() -> std::path::PathBuf {
    if let Some(root) = std::env::var_os("SystemRoot").or_else(|| std::env::var_os("windir")) {
        let p = std::path::PathBuf::from(root)
            .join("System32")
            .join("cmd.exe");
        if p.is_file() {
            return p;
        }
    }
    std::path::PathBuf::from("cmd.exe")
}

/// Build a `Command` for a resolved [`Spawnable`] with safe argv. A
/// `Direct` exe is invoked straight; a `BatchShim` is run via
/// `cmd.exe /C <shim> <args…>` with each arg as a discrete element (no
/// shell-string concatenation — a Brief's content cannot inject).
fn command_for(spawn: &Spawnable, args: &[String]) -> std::process::Command {
    use std::process::Command;
    match spawn {
        Spawnable::Direct(path) => {
            let mut c = Command::new(path);
            c.args(args);
            c
        }
        Spawnable::BatchShim(path) => {
            let mut c = Command::new(trusted_cmd_exe());
            c.arg("/C").arg(path).args(args);
            c
        }
    }
}

// The standard CLI Rigs, as ProcessRigs. Each spawns the operator's
// installed CLI, which authenticates with ITS OWN subscription
// login — **no inference key is injected**. This is the
// subscription model from `docs/relix-agent-adapters.md`: run heavy
// agents on a flat-rate Claude Max / ChatGPT (Codex) / Gemini
// subscription instead of metered API. The flags here are the
// starting shape; future refinements add availability / login
// probing and structured-output parsing.

/// Absolute fallback paths to a real `claude` executable that PATH may
/// not surface. On Windows, npm installs Claude Code as a `claude.cmd`
/// shim on PATH but ships the real launcher at
/// `%APPDATA%\npm\node_modules\@anthropic-ai\claude-code\bin\claude.exe`
/// — a directly-spawnable `.exe` the resolver prefers over the shim.
fn claude_fallback_paths() -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    if cfg!(windows)
        && let Some(appdata) = std::env::var_os("APPDATA")
    {
        v.push(
            std::path::PathBuf::from(appdata)
                .join("npm")
                .join("node_modules")
                .join("@anthropic-ai")
                .join("claude-code")
                .join("bin")
                .join("claude.exe"),
        );
    }
    v
}

/// Claude Code on a Claude subscription. Prompt piped to stdin.
///
/// Readiness is a TWO-step check: `claude --version` (installed + runs)
/// then `claude auth status --text` (logged in). On Windows the binary
/// is resolved through PATH+PATHEXT (the `claude.cmd` npm shim) with a
/// fallback to the real npm `claude.exe`, so a working install is never
/// misreported as `probe_failed`.
pub fn claude_rig() -> ProcessRig {
    let mut rig = ProcessRig::new(
        "claude",
        "claude",
        vec![
            "--print".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
        ],
    )
    .with_structured_output(true)
    .with_output_format(RigOutputFormat::ClaudeStreamJson)
    .with_billing(RigBilling::subscription("anthropic", "5h/weekly"))
    .with_install_hint("install Claude Code (npm i -g @anthropic-ai/claude-code), then `claude auth login`")
    .with_readiness(
        vec!["--version".to_string()],
        "run `claude auth login` to authenticate (check with `claude auth status --text`)",
    )
    .with_auth_probe(vec![
        "auth".to_string(),
        "status".to_string(),
        "--text".to_string(),
    ]);
    for p in claude_fallback_paths() {
        rig = rig.with_fallback_path(p);
    }
    rig
}

/// Codex on a ChatGPT / Codex subscription. Prompt piped via the
/// trailing `-` (read from stdin).
pub fn codex_rig() -> ProcessRig {
    ProcessRig::new(
        "codex",
        "codex",
        vec!["exec".to_string(), "--json".to_string(), "-".to_string()],
    )
    .with_structured_output(true)
    .with_billing(RigBilling::subscription("openai", "5h/weekly/credits"))
    .with_install_hint("install Codex CLI, then run `codex login`")
    .with_readiness(vec!["--version".to_string()], "run `codex login` to authenticate")
}

/// Gemini CLI on a Google subscription. Prompt piped to stdin.
pub fn gemini_rig() -> ProcessRig {
    ProcessRig::new("gemini", "gemini", Vec::new())
        .with_billing(RigBilling::subscription("google", "provider-window"))
        .with_install_hint("install Gemini CLI, then authenticate it")
        .with_readiness(vec!["--version".to_string()], "authenticate the Gemini CLI")
}

/// An installed **Hermes** agent, plugged in as a Rig (Pillar 2 —
/// the deepest "plug in any agent" target).
///
/// IMPORTANT: this is a **stdio placeholder**, governed `BoxLevel`.
/// A plain process over stdin/stdout is a black box — Relix can only
/// gate it at the box wall, so per the adapters §6 invariant
/// ("governance reflects what Relix can actually gate") it must be
/// `BoxLevel`, NOT `PerToolCall`. The *real* Hermes adapter the docs
/// describe (relix-hermes-integration §2.2: the structured `/v1/runs`
/// HTTP seam + gated tools over MCP + the `relix-bridge` in-Hermes
/// plugin with `pre_tool_call`/`pre_approval` hooks) is what earns
/// `PerToolCall`; it is not built yet. Do not relabel this stdio
/// path as `PerToolCall` until that rich transport exists.
pub fn hermes_rig() -> ProcessRig {
    ProcessRig::new("hermes", "hermes", vec!["run".to_string(), "-".to_string()])
        .with_install_hint("install Hermes and ensure `hermes` is on PATH")
    // governance left at the conservative BoxLevel default (see above)
}

/// Register the standard CLI subscription Rigs into `registry`.
/// They spawn external binaries, so a Rig whose CLI isn't installed
/// simply fails gracefully at run time (a retryable `Failed`) — the
/// operator opts an Operative onto one by setting its `rig`.
pub fn register_cli_rigs(registry: &mut RigRegistry) {
    registry.register(Arc::new(claude_rig()));
    registry.register(Arc::new(codex_rig()));
    registry.register(Arc::new(gemini_rig()));
    registry.register(Arc::new(hermes_rig()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echo_rig_runs_and_reports_done() {
        let rig = EchoRig;
        assert_eq!(rig.name(), "echo");
        assert_eq!(rig.governance(), RigGovernance::BoxLevel);
        let req = RigRunRequest::new("brief_1", "agt_a", "guild_x", "write the readme")
            .with_context("goal: ship v1");
        match rig.run(&req) {
            RigOutcome::Done { summary } => assert_eq!(summary, "echo: write the readme"),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn echo_rig_fails_on_empty_prompt() {
        let rig = EchoRig;
        let req = RigRunRequest::new("b", "a", "g", "   ");
        assert!(matches!(
            rig.run(&req),
            RigOutcome::Failed {
                retryable: false,
                ..
            }
        ));
    }

    #[test]
    fn registry_registers_looks_up_and_overrides() {
        let reg = RigRegistry::with_builtins();
        assert_eq!(reg.names(), vec!["echo".to_string()]);
        assert!(reg.get("echo").is_some());
        assert!(reg.get("nope").is_none());
        assert_eq!(reg.get("echo").unwrap().name(), "echo");

        // A custom Rig registers the same way; same name overrides.
        struct CustomEcho;
        impl Rig for CustomEcho {
            fn name(&self) -> &str {
                "echo"
            }
            fn governance(&self) -> RigGovernance {
                RigGovernance::PerToolCall
            }
            fn run(&self, _req: &RigRunRequest) -> RigOutcome {
                RigOutcome::Continue {
                    note: "custom".to_string(),
                }
            }
        }
        let mut reg = reg;
        reg.register(Arc::new(CustomEcho));
        assert_eq!(reg.len(), 1, "override keeps a single 'echo' entry");
        assert_eq!(
            reg.get("echo").unwrap().governance(),
            RigGovernance::PerToolCall
        );
    }

    // Cross-platform command helpers for the ProcessRig tests.
    fn echo_cmd(s: &str) -> (String, Vec<String>) {
        if cfg!(windows) {
            ("cmd".into(), vec!["/C".into(), "echo".into(), s.into()])
        } else {
            ("sh".into(), vec!["-c".into(), format!("echo {s}")])
        }
    }
    fn fail_cmd() -> (String, Vec<String>) {
        if cfg!(windows) {
            ("cmd".into(), vec!["/C".into(), "exit".into(), "1".into()])
        } else {
            ("sh".into(), vec!["-c".into(), "exit 1".into()])
        }
    }
    fn echo_env_cmd(var: &str) -> (String, Vec<String>) {
        if cfg!(windows) {
            ("cmd".into(), vec!["/C".into(), format!("echo %{var}%")])
        } else {
            ("sh".into(), vec!["-c".into(), format!("echo ${var}")])
        }
    }

    #[test]
    fn process_rig_injects_then_redacts_the_bridge_token() {
        // The token IS injected into the child env (the child echoes it),
        // and the captured output REDACTS it so it never reaches the
        // Chronicle / dashboard. Seeing `***` proves both happened.
        let (prog, args) = echo_env_cmd("RELIX_BRIDGE_TOKEN");
        let rig = ProcessRig::new("test-env", prog, args);
        let req = RigRunRequest::new("brief_1", "agt_a", "g", "ignored")
            .with_bridge_token("brt_secret123long_enough");
        match rig.run(&req) {
            RigOutcome::Done { summary } => {
                assert!(!summary.contains("brt_secret123long_enough"), "token leaked: {summary:?}");
                assert!(summary.contains("***"), "token should be redacted: {summary:?}");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn process_rig_runs_a_command_and_captures_stdout() {
        let (prog, args) = echo_cmd("hello-from-rig");
        let rig = ProcessRig::new("test-echo", prog, args);
        let req = RigRunRequest::new("b", "a", "g", "ignored stdin");
        match rig.run(&req) {
            RigOutcome::Done { summary } => {
                assert!(summary.contains("hello-from-rig"), "got: {summary:?}")
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn process_rig_maps_nonzero_exit_to_retryable_failed() {
        let (prog, args) = fail_cmd();
        let rig = ProcessRig::new("test-fail", prog, args);
        let req = RigRunRequest::new("b", "a", "g", "x");
        assert!(matches!(
            rig.run(&req),
            RigOutcome::Failed {
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn process_rig_maps_missing_program_to_non_retryable_failed() {
        // A program that resolves to nothing is detected BEFORE spawn and
        // reported as a clear, non-retryable "not found" (retrying won't
        // conjure an uninstalled binary) — not a transient spawn blip.
        let rig = ProcessRig::new("nope", "this-binary-does-not-exist-xyzzy", vec![]);
        let req = RigRunRequest::new("b", "a", "g", "x");
        match rig.run(&req) {
            RigOutcome::Failed { retryable, reason } => {
                assert!(!retryable, "a missing binary is not retryable");
                assert!(reason.contains("not found"), "reason: {reason}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    fn sleep_cmd(secs: u32) -> (String, Vec<String>) {
        if cfg!(windows) {
            // `timeout` needs a console; `ping` is the portable sleeper.
            ("cmd".into(), vec!["/C".into(), format!("ping -n {} 127.0.0.1 >NUL", secs + 1)])
        } else {
            ("sh".into(), vec!["-c".into(), format!("sleep {secs}")])
        }
    }

    #[test]
    fn process_rig_times_out_and_kills_the_child() {
        // A child that sleeps far longer than the timeout must be killed
        // and reported as a retryable timeout — not hang the worker.
        let (prog, args) = sleep_cmd(30);
        let rig = ProcessRig::new("slow", prog, args)
            .with_timeout(std::time::Duration::from_millis(400));
        let started = std::time::Instant::now();
        let outcome = rig.run(&RigRunRequest::new("b", "a", "g", "x"));
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "should not hang");
        match outcome {
            RigOutcome::Failed { retryable, reason } => {
                assert!(retryable);
                assert!(reason.contains("timed out"), "got: {reason}");
            }
            other => panic!("expected timeout Failed, got {other:?}"),
        }
    }

    #[test]
    fn process_rig_rejects_missing_working_dir_non_retryably() {
        let rig = ProcessRig::new("p", "echo", vec![]).with_working_dir("/relix/no/such/dir/xyzzy");
        match rig.run(&RigRunRequest::new("b", "a", "g", "x")) {
            RigOutcome::Failed { retryable, reason } => {
                assert!(!retryable, "missing dir is a hard error");
                assert!(reason.contains("working dir"), "got: {reason}");
            }
            other => panic!("expected dir Failed, got {other:?}"),
        }
    }

    #[test]
    fn process_rig_honours_per_run_working_dir() {
        // pwd-equivalent in the temp dir; the child should run there.
        let tmp = tempfile::tempdir().unwrap();
        let canon = std::fs::canonicalize(tmp.path()).unwrap();
        let (prog, args) = if cfg!(windows) {
            ("cmd".to_string(), vec!["/C".into(), "cd".into()])
        } else {
            ("sh".to_string(), vec!["-c".into(), "pwd".into()])
        };
        let rig = ProcessRig::new("cwd", prog, args);
        let req = RigRunRequest::new("b", "a", "g", "x").with_working_dir(canon.clone());
        match rig.run(&req) {
            RigOutcome::Done { summary } => {
                let leaf = canon.file_name().unwrap().to_string_lossy().to_string();
                assert!(summary.contains(&leaf), "cwd {summary:?} should contain {leaf}");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn process_rig_passes_args_literally_no_shell_injection() {
        // Args with shell metacharacters are passed as one literal argv
        // entry — never interpreted by a shell. echo prints it verbatim.
        let payload = "x; rm -rf / && echo pwned `whoami`";
        let (prog, args) = echo_cmd(payload);
        // The metacharacters live inside a single argv element, so there
        // is no shell to act on them: the ProcessRig spawns the program
        // directly (Command::new + args), not `sh -c <string>`.
        let rig = ProcessRig::new("safe", prog, args.clone());
        assert_eq!(rig.args(), args.as_slice());
        // And running it just echoes the literal (no `pwned`, no deletion).
        if let RigOutcome::Done { summary } = rig.run(&RigRunRequest::new("b", "a", "g", "x")) {
            assert!(summary.contains("rm -rf"), "literal text preserved: {summary:?}");
        }
    }

    #[test]
    fn redact_secrets_masks_tokens_and_preserves_formatting() {
        let bt = "deadbeefdeadbeef00000000";
        let input = "ok line\nbridge=deadbeefdeadbeef00000000\nkey FAKE_TEST_FIXTURE_REDACTED\nplain word\nOPENAI_API_KEY=supersecretvalue\n";
        let out = redact_secrets(input, bt);
        assert!(!out.contains(bt), "bridge token leaked: {out}");
        assert!(!out.contains("FAKE_TEST_FIXTURE_REDACTED"), "sk- token leaked: {out}");
        assert!(!out.contains("supersecretvalue"), "env secret leaked: {out}");
        assert!(out.contains("OPENAI_API_KEY=***"), "env name kept + masked: {out}");
        // Formatting (newlines, the safe words) survives.
        assert_eq!(out.lines().count(), input.lines().count());
        assert!(out.contains("plain word"));
        assert!(out.contains("ok line"));
    }

    #[test]
    fn timeout_clamped_to_at_least_one_second() {
        let rig = ProcessRig::new("p", "echo", vec![]).with_timeout(std::time::Duration::ZERO);
        assert!(rig.timeout() >= std::time::Duration::from_secs(1));
    }

    #[test]
    fn cli_rig_factories_configure_the_right_commands() {
        let c = claude_rig();
        assert_eq!(c.name(), "claude");
        assert_eq!(c.program(), "claude");
        assert!(c.args().iter().any(|a| a == "--print"));
        assert!(c.args().iter().any(|a| a == "--output-format"));
        assert!(c.args().iter().any(|a| a == "stream-json"));
        assert!(c.structured_output());
        assert_eq!(c.billing().mode, "subscription");
        assert_eq!(c.billing().provider.as_deref(), Some("anthropic"));

        let x = codex_rig();
        assert_eq!(x.name(), "codex");
        assert_eq!(x.program(), "codex");
        assert!(x.args().iter().any(|a| a == "exec"));
        assert!(x.args().iter().any(|a| a == "--json"));
        assert!(x.structured_output());
        assert_eq!(x.billing().mode, "subscription");
        assert_eq!(x.billing().provider.as_deref(), Some("openai"));

        assert_eq!(gemini_rig().name(), "gemini");
        assert_eq!(gemini_rig().billing().mode, "subscription");

        // Hermes stdio placeholder: BoxLevel until the real
        // /v1/runs+MCP+plugin seam (which earns PerToolCall) is built.
        let h = hermes_rig();
        assert_eq!(h.name(), "hermes");
        assert_eq!(h.program(), "hermes");
        assert_eq!(h.governance(), RigGovernance::BoxLevel);
    }

    #[test]
    fn register_cli_rigs_adds_them_alongside_builtins() {
        let mut reg = RigRegistry::with_builtins();
        register_cli_rigs(&mut reg);
        for name in ["echo", "claude", "codex", "gemini", "hermes"] {
            assert!(reg.get(name).is_some(), "{name} should be registered");
        }
    }

    #[test]
    fn process_rig_governance_defaults_box_and_opts_up() {
        // Default: a plain process is a black box.
        let plain = ProcessRig::new("p", "true", vec![]);
        assert_eq!(plain.governance(), RigGovernance::BoxLevel);
        // Opt up when the adapter surfaces its tool calls.
        let rich =
            ProcessRig::new("h", "hermes", vec![]).with_governance(RigGovernance::PerToolCall);
        assert_eq!(rich.governance(), RigGovernance::PerToolCall);
    }

    #[test]
    fn process_rig_probe_reports_missing_program_with_hint() {
        let rig = ProcessRig::new(
            "missing",
            "definitely-not-installed-relix-rig-test-binary",
            vec![],
        )
        .with_install_hint("install the missing adapter");
        let probe = rig.probe();
        assert_eq!(probe.status, "missing_binary");
        assert!(!probe.is_available());
        assert!(probe.detail.contains("definitely-not-installed"));
        assert_eq!(
            probe.install_hint.as_deref(),
            Some("install the missing adapter")
        );
    }

    // ── Rich CLI readiness classification (mocked command outputs) ──

    fn sig(exit_ok: bool, stdout: &str, stderr: &str) -> ReadinessSignals {
        ReadinessSignals {
            missing_binary: false,
            timed_out: false,
            spawn_error: None,
            exit_ok,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    #[test]
    fn classify_readiness_available_from_clean_version() {
        let (status, detail) = classify_readiness(&sig(true, "claude 1.2.3 (Claude Code)", ""));
        assert_eq!(status, "available");
        assert!(detail.contains("1.2.3"), "got: {detail}");
    }

    #[test]
    fn classify_readiness_missing_binary() {
        let s = ReadinessSignals { missing_binary: true, ..Default::default() };
        assert_eq!(classify_readiness(&s).0, "missing_binary");
    }

    #[test]
    fn classify_readiness_spawn_error_is_probe_failed() {
        let s = ReadinessSignals {
            spawn_error: Some("permission denied".into()),
            ..Default::default()
        };
        let (status, detail) = classify_readiness(&s);
        assert_eq!(status, "probe_failed");
        assert!(detail.contains("permission denied"));
    }

    #[test]
    fn classify_readiness_timeout_is_interactive_only() {
        let s = ReadinessSignals { timed_out: true, ..Default::default() };
        assert_eq!(classify_readiness(&s).0, "interactive_only");
    }

    #[test]
    fn classify_readiness_auth_keywords_are_not_authenticated() {
        for (out, err) in [
            ("", "Error: Not authenticated. Please run `claude login`."),
            ("", "you are not signed in"),
            ("error: 401 Unauthorized", ""),
            ("Please log in to continue", ""),
            ("", "login required: run `codex login`"),
        ] {
            // Auth keywords win even when exit looked ok.
            assert_eq!(
                classify_readiness(&sig(true, out, err)).0,
                "not_authenticated",
                "out={out:?} err={err:?}"
            );
        }
    }

    #[test]
    fn classify_readiness_unknown_flag_is_unsupported_version() {
        let (status, _) = classify_readiness(&sig(false, "", "error: unknown flag: --version"));
        assert_eq!(status, "unsupported_version");
    }

    #[test]
    fn classify_readiness_other_failure_is_probe_failed() {
        let (status, detail) = classify_readiness(&sig(false, "", "segfault in libfoo"));
        assert_eq!(status, "probe_failed");
        assert!(detail.contains("segfault"));
    }

    #[test]
    fn run_readiness_probe_missing_binary_reports_missing() {
        let s = run_readiness_probe(
            "definitely-not-installed-relix-probe-xyzzy",
            &["--version".to_string()],
            std::time::Duration::from_secs(2),
        );
        assert!(s.missing_binary);
        assert_eq!(classify_readiness(&s).0, "missing_binary");
    }

    #[test]
    fn run_readiness_probe_runs_real_command_and_captures_stdout() {
        // A real, always-available command echoes a version-like line.
        let (prog, args) = if cfg!(windows) {
            ("cmd".to_string(), vec!["/C".to_string(), "echo".to_string(), "probe-ok 9.9".to_string()])
        } else {
            ("sh".to_string(), vec!["-c".to_string(), "echo probe-ok 9.9".to_string()])
        };
        let s = run_readiness_probe(&prog, &args, std::time::Duration::from_secs(5));
        assert!(!s.missing_binary && !s.timed_out && s.exit_ok, "signals: {s:?}");
        assert!(s.stdout.contains("probe-ok"), "stdout: {:?}", s.stdout);
        assert_eq!(classify_readiness(&s).0, "available");
    }

    #[test]
    fn process_rig_caps_stdout() {
        let long = "x".repeat(1000);
        let rig = if cfg!(windows) {
            ProcessRig::new("p", "cmd", vec!["/C".into(), format!("echo {long}")])
        } else {
            ProcessRig::new("p", "sh", vec!["-c".into(), format!("printf '{long}'")])
        }
        .with_max_output_bytes(10);
        assert_eq!(rig.max_output_bytes(), 10);

        let req = RigRunRequest::new("b", "a", "g", "prompt");
        match rig.run(&req) {
            RigOutcome::Done { summary } => {
                assert!(summary.len() <= 10, "summary len {}", summary.len());
            }
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn resolve_prefers_the_agents_rig_then_falls_back_to_default() {
        let reg = RigRegistry::with_builtins().with_default("echo");
        assert_eq!(reg.default_name(), Some("echo"));

        // Preferred + known → that Rig.
        assert_eq!(reg.resolve(Some("echo")).unwrap().name(), "echo");
        // Preferred but unknown → fall back to default.
        assert_eq!(reg.resolve(Some("ghost")).unwrap().name(), "echo");
        // None / empty preferred → default.
        assert_eq!(reg.resolve(None).unwrap().name(), "echo");
        assert_eq!(reg.resolve(Some("")).unwrap().name(), "echo");

        // No default configured → unknown/none resolves to nothing.
        let bare = RigRegistry::with_builtins();
        assert!(bare.resolve(Some("ghost")).is_none());
        assert!(bare.resolve(None).is_none());
        assert_eq!(bare.resolve(Some("echo")).unwrap().name(), "echo");
    }

    #[test]
    fn describe_reports_name_label_and_governance_sorted() {
        let mut reg = RigRegistry::with_builtins();
        register_cli_rigs(&mut reg);
        let infos = reg.describe();
        // One entry per registered Rig, sorted by name (BTreeMap).
        assert_eq!(infos.len(), reg.len());
        let names: Vec<&str> = infos.iter().map(|i| i.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        // echo is thin (box-level) by default.
        let echo = infos.iter().find(|i| i.name == "echo").unwrap();
        assert_eq!(echo.governance, "box_level");
        assert!(!echo.bridge_back);
        assert!(!echo.structured_output);
        assert_eq!(echo.billing.mode, "none");
        assert_eq!(echo.probe.status, "available");
        let claude = infos.iter().find(|i| i.name == "claude").unwrap();
        assert!(claude.bridge_back);
        assert!(claude.structured_output);
        assert_eq!(claude.billing.mode, "subscription");
        assert_eq!(claude.billing.provider.as_deref(), Some("anthropic"));
        // The CLI probe runs live, so the exact status depends on the host
        // (installed / needs-login / not present). It must be one of the
        // structured statuses, and any non-available status carries a hint.
        assert!(matches!(
            claude.probe.status.as_str(),
            "available"
                | "missing_binary"
                | "not_authenticated"
                | "unsupported_version"
                | "interactive_only"
                | "probe_failed"
        ));
        if !claude.probe.is_available() {
            assert!(claude.probe.install_hint.is_some());
        }
        // JSON-serialisable for the agent-config UI.
        let json = serde_json::to_string(&infos).unwrap();
        assert!(json.contains("box_level"));
        assert!(json.contains("subscription_included"));
    }

    // ── Windows-safe executable resolution ──────────────────────

    #[cfg(windows)]
    #[test]
    fn windows_cmd_shim_resolves_and_spawns_via_cmd_exe() {
        // An npm shim on PATH (no real .exe) — the exact Claude-on-Windows
        // case. It must resolve to a BatchShim and spawn through cmd.exe.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::File::create(tmp.path().join("claude.cmd")).unwrap();
        let exts = vec![String::new(), ".exe".into(), ".cmd".into()];
        let s = resolve_in_dirs("claude", &[tmp.path().to_path_buf()], &exts, &[])
            .expect("the .cmd shim must resolve");
        assert!(
            matches!(&s, Spawnable::BatchShim(p) if p.ends_with("claude.cmd")),
            "got {s:?}"
        );
        // Spawned via `cmd.exe /C <shim> <args…>` with discrete argv.
        let cmd = command_for(&s, &["--version".to_string()]);
        let prog = cmd.get_program().to_string_lossy().to_ascii_lowercase();
        assert!(prog.ends_with("cmd.exe"), "prog={prog}");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "/C");
        assert!(args[1].to_ascii_lowercase().ends_with("claude.cmd"));
        assert_eq!(args[2], "--version");
    }

    #[cfg(windows)]
    #[test]
    fn windows_direct_exe_preferred_over_cmd_shim() {
        // A dir holding BOTH tool.cmd and tool.exe → the real .exe wins.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::File::create(tmp.path().join("tool.cmd")).unwrap();
        std::fs::File::create(tmp.path().join("tool.exe")).unwrap();
        let exts = vec![String::new(), ".exe".into(), ".cmd".into()];
        let s = resolve_in_dirs("tool", &[tmp.path().to_path_buf()], &exts, &[]).unwrap();
        assert!(
            matches!(&s, Spawnable::Direct(p) if p.extension().unwrap() == "exe"),
            "got {s:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_fallback_exe_beats_path_shim() {
        // PATH has only the .cmd shim; the npm real claude.exe is the
        // fallback. The directly-spawnable .exe must be preferred.
        let path_dir = tempfile::tempdir().unwrap();
        std::fs::File::create(path_dir.path().join("claude.cmd")).unwrap();
        let fb_dir = tempfile::tempdir().unwrap();
        let fb = fb_dir.path().join("claude.exe");
        std::fs::File::create(&fb).unwrap();
        let exts = vec![String::new(), ".exe".into(), ".cmd".into()];
        let s = resolve_in_dirs(
            "claude",
            &[path_dir.path().to_path_buf()],
            &exts,
            &[fb.clone()],
        )
        .unwrap();
        assert_eq!(s, Spawnable::Direct(fb), "the real .exe fallback should win over the .cmd shim");
    }

    #[cfg(windows)]
    #[test]
    fn windows_classify_file_by_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("a.exe");
        std::fs::File::create(&exe).unwrap();
        let cmd = tmp.path().join("a.cmd");
        std::fs::File::create(&cmd).unwrap();
        let bat = tmp.path().join("a.bat");
        std::fs::File::create(&bat).unwrap();
        // The npm-shim trap: an EXTENSIONLESS file (the `claude` sh shim
        // that lives next to `claude.cmd`) is NOT a Windows executable and
        // must classify as None — spawning it directly is `os error 193`.
        let noext = tmp.path().join("claude");
        std::fs::File::create(&noext).unwrap();
        assert!(matches!(classify_file(&exe), Some(Spawnable::Direct(_))));
        assert!(matches!(classify_file(&cmd), Some(Spawnable::BatchShim(_))));
        assert!(matches!(classify_file(&bat), Some(Spawnable::BatchShim(_))));
        assert!(classify_file(&noext).is_none(), "extensionless sh shim must not be Direct");
        assert!(classify_file(&tmp.path().join("missing.exe")).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_skips_extensionless_shim_and_uses_real_exe() {
        // The exact failing layout: a PATH dir with the `claude` sh shim +
        // `claude.cmd`, and the real npm `claude.exe` as a fallback. The
        // resolver must skip the sh shim and pick the real .exe.
        let path_dir = tempfile::tempdir().unwrap();
        std::fs::File::create(path_dir.path().join("claude")).unwrap(); // sh shim
        std::fs::File::create(path_dir.path().join("claude.cmd")).unwrap();
        let fb_dir = tempfile::tempdir().unwrap();
        let real = fb_dir.path().join("claude.exe");
        std::fs::File::create(&real).unwrap();
        let exts = path_search_exts();
        let s = resolve_in_dirs("claude", &[path_dir.path().to_path_buf()], &exts, &[real.clone()])
            .unwrap();
        assert_eq!(s, Spawnable::Direct(real), "must skip the sh shim and use the real .exe");
    }

    #[cfg(unix)]
    #[test]
    fn unix_resolves_bare_name_to_direct() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("mytool");
        std::fs::File::create(&bin).unwrap();
        let s = resolve_in_dirs("mytool", &[tmp.path().to_path_buf()], &[String::new()], &[]).unwrap();
        assert_eq!(s, Spawnable::Direct(bin));
    }

    // ── Claude two-step (version + auth) readiness classification ──

    #[test]
    fn claude_logged_in_auth_status_is_available() {
        let v = sig(true, "2.1.159 (Claude Code)", "");
        let auth = sig(true, "Logged in\nAccount: a@b.com\nPlan: Claude Max", "");
        let (status, detail) = classify_readiness_with_auth(&v, Some(&auth));
        assert_eq!(status, "available", "detail={detail}");
    }

    #[test]
    fn claude_logged_out_auth_status_is_not_authenticated() {
        let v = sig(true, "2.1.159 (Claude Code)", "");
        for auth in [
            sig(true, "Not logged in. Run `claude auth login` to sign in.", ""),
            sig(false, "", "You are not signed in"),
            sig(true, "unauthenticated", ""),
        ] {
            assert_eq!(
                classify_readiness_with_auth(&v, Some(&auth)).0,
                "not_authenticated",
                "auth={auth:?}"
            );
        }
    }

    #[test]
    fn claude_auth_status_hang_is_interactive_only() {
        let v = sig(true, "2.1.159 (Claude Code)", "");
        let auth = ReadinessSignals { timed_out: true, ..Default::default() };
        assert_eq!(
            classify_readiness_with_auth(&v, Some(&auth)).0,
            "interactive_only"
        );
    }

    #[test]
    fn auth_unavailable_or_absent_does_not_block_installed_binary() {
        let v = sig(true, "2.1.159 (Claude Code)", "");
        // An older CLI lacking `auth status` (the auth probe can't spawn)
        // must not block a clearly-installed binary.
        let auth = ReadinessSignals {
            spawn_error: Some("not a subcommand".into()),
            ..Default::default()
        };
        assert_eq!(classify_readiness_with_auth(&v, Some(&auth)).0, "available");
        // No auth probe configured at all → the version verdict stands.
        assert_eq!(classify_readiness_with_auth(&v, None).0, "available");
    }

    #[test]
    fn spawn_failure_is_probe_failed_not_missing_install() {
        // The ORIGINAL bug: a resolvable-but-unspawnable program looked
        // like it "could not spawn" and was reported probe_failed — but it
        // must NEVER be classified missing_binary (which would wrongly
        // tell the operator to install something already present).
        let v = ReadinessSignals {
            spawn_error: Some("program not found".into()),
            ..Default::default()
        };
        let (status, _) = classify_readiness_with_auth(&v, None);
        assert_eq!(status, "probe_failed");
        assert_ne!(status, "missing_binary");
    }

    // ── Claude stream-json result parsing ──────────────────────

    // A representative slice of `claude --print --output-format
    // stream-json --verbose` stdout: hook/system noise, an assistant
    // event, then the terminal `result` event (the only one we read).
    fn claude_jsonl(result_obj: &str) -> String {
        format!(
            "{}\n{}\n{}\n",
            r#"{"type":"system","subtype":"hook_started","hook_name":"SessionStart:startup"}"#,
            r#"{"type":"assistant","message":{"role":"assistant"}}"#,
            result_obj,
        )
    }

    #[test]
    fn parse_claude_stream_json_extracts_terminal_result() {
        let jsonl = claude_jsonl(
            r#"{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"Relix Claude test passed","permission_denials":[]}"#,
        );
        let r = parse_claude_stream_json(&jsonl).expect("a result event");
        assert_eq!(r.text, "Relix Claude test passed");
        assert!(!r.is_error);
        assert_eq!(r.subtype, "success");
        assert_eq!(r.permission_denials, 0);
        assert_eq!(r.num_turns, 1);
    }

    #[test]
    fn parse_claude_stream_json_reads_permission_denials() {
        let jsonl = claude_jsonl(
            r#"{"type":"result","subtype":"success","is_error":false,"num_turns":2,"result":"Created the note pending approval","permission_denials":[{"tool":"Write"}]}"#,
        );
        let r = parse_claude_stream_json(&jsonl).unwrap();
        assert_eq!(r.permission_denials, 1);
        assert_eq!(r.num_turns, 2);
    }

    #[test]
    fn parse_claude_stream_json_none_without_result_event() {
        // Interrupted run — system lines only, no terminal result.
        let jsonl = format!(
            "{}\n{}\n",
            r#"{"type":"system","subtype":"hook_started"}"#,
            r#"{"type":"assistant"}"#,
        );
        assert!(parse_claude_stream_json(&jsonl).is_none());
        // And junk / non-JSON lines are skipped without panicking.
        assert!(parse_claude_stream_json("not json\n\n{bad").is_none());
    }

    fn claude_test_rig() -> ProcessRig {
        ProcessRig::new("claude", "claude", vec![])
            .with_output_format(RigOutputFormat::ClaudeStreamJson)
    }

    #[test]
    fn claude_outcome_success_returns_clean_answer() {
        let jsonl = claude_jsonl(
            r#"{"type":"result","subtype":"success","is_error":false,"num_turns":1,"result":"Relix Claude test passed","permission_denials":[]}"#,
        );
        match claude_test_rig().claude_outcome(&jsonl, "") {
            Some(RigOutcome::Done { summary }) => {
                assert_eq!(summary, "Relix Claude test passed", "no JSONL noise in the summary");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn claude_outcome_permission_denial_surfaces_warning() {
        let jsonl = claude_jsonl(
            r#"{"type":"result","subtype":"success","is_error":false,"num_turns":2,"result":"Created the note pending approval.","permission_denials":[{"tool":"Write"}]}"#,
        );
        match claude_test_rig().claude_outcome(&jsonl, "") {
            Some(RigOutcome::Done { summary }) => {
                assert!(summary.contains("permission(s) denied"), "got: {summary}");
                assert!(summary.contains("Created the note"), "keeps the model reply: {summary}");
            }
            other => panic!("expected Done with a denial caveat, got {other:?}"),
        }
    }

    #[test]
    fn claude_outcome_is_error_is_a_clear_failure() {
        let jsonl = claude_jsonl(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":3,"result":"something went wrong","permission_denials":[]}"#,
        );
        match claude_test_rig().claude_outcome(&jsonl, "") {
            Some(RigOutcome::Failed { reason, retryable }) => {
                assert!(!retryable);
                assert!(reason.contains("error_during_execution"), "reason: {reason}");
                assert!(reason.contains("something went wrong"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn claude_outcome_none_without_result_falls_through() {
        // No terminal result → None, so run() falls back to exit-code
        // handling (never silently claims success).
        let jsonl = r#"{"type":"system","subtype":"init"}"#;
        assert!(claude_test_rig().claude_outcome(jsonl, "").is_none());
    }

    #[test]
    fn claude_rig_uses_stream_json_parser() {
        assert_eq!(claude_rig().output_format, RigOutputFormat::ClaudeStreamJson);
    }

    #[test]
    fn claude_rig_uses_two_step_readiness_and_windows_fallback() {
        let rig = claude_rig();
        let r = rig.readiness.as_ref().expect("claude has a readiness check");
        assert_eq!(r.probe_args, vec!["--version".to_string()]);
        assert_eq!(
            r.auth_args.as_deref(),
            Some(&["auth".to_string(), "status".to_string(), "--text".to_string()][..])
        );
        assert!(r.login_hint.contains("claude auth login"), "hint: {}", r.login_hint);
        assert!(
            rig.install_hint.as_deref().unwrap().contains("claude auth login"),
            "install hint should reference auth login"
        );
        if cfg!(windows) {
            assert!(
                rig.fallback_paths
                    .iter()
                    .any(|p| p.to_string_lossy().contains("claude.exe")),
                "windows claude should carry an npm claude.exe fallback: {:?}",
                rig.fallback_paths
            );
        }
    }
}
