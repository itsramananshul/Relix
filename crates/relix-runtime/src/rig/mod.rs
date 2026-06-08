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
    /// The durable run id (`brief_runs.run_id`). Used to poll the
    /// [`CancelRegistry`] so an operator can stop an in-flight run, and to
    /// key the run's transcript. Empty for ad-hoc / timer runs.
    pub run_id: String,
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
    /// Optional per-run model override carried from the assigned
    /// Operative's stored `model_preference` (relix-agent-adapters.md
    /// §3.2/§3.3). Empty/absent → the adapter runs on its own default
    /// model. A supported subscription CLI Rig maps this to its `--model`
    /// flag (Claude + Codex); echo / raw / unsupported Rigs ignore it, so
    /// the field is fully backward-compatible.
    pub model_preference: Option<String>,
    /// Optional reasoning/effort tier carried from the Operative's stored
    /// `reasoning_effort` (`minimal`/`low`/`medium`/`high`). Only the
    /// Codex Rig maps this (`-c model_reasoning_effort=<effort>`, adapters
    /// §3.3); other Rigs ignore it.
    pub reasoning_effort: Option<String>,
    /// Optional resumable adapter session id, looked up from the SAME
    /// (tenant, Operative, Rig, Brief) runtime state currently stored
    /// (relix-agent-adapters.md §3.3). Empty/absent → the adapter starts a
    /// fresh session. Only the Codex Rig maps this (`codex exec resume
    /// [OPTIONS] <session> -`); the Claude Rig deliberately does NOT (its
    /// working-dir-keyed session store does not survive Relix's per-run
    /// scoped workspace — see `argv_with_resume`); echo / raw / Gemini /
    /// generic Rigs ignore it, so the field is fully backward-compatible.
    pub resume_session_id: Option<String>,
}

/// Trim a stored preference and collapse empty/whitespace-only to `None`
/// so an absent or blank preference never reaches an adapter as a flag.
fn normalize_pref(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

impl RigRunRequest {
    pub fn new(
        brief_id: impl Into<String>,
        agent_id: impl Into<String>,
        tenant_id: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            run_id: String::new(),
            brief_id: brief_id.into(),
            agent_id: agent_id.into(),
            tenant_id: tenant_id.into(),
            prompt: prompt.into(),
            context: String::new(),
            bridge_token: String::new(),
            working_dir: None,
            model_preference: None,
            reasoning_effort: None,
            resume_session_id: None,
        }
    }

    /// Set the durable run id (builder style) — enables cancellation.
    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = run_id.into();
        self
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

    /// Carry the Operative's stored model preference (builder style).
    /// Empty / whitespace-only normalizes to `None` so a blank preference
    /// is indistinguishable from an absent one.
    pub fn with_model_preference(mut self, model: Option<String>) -> Self {
        self.model_preference = normalize_pref(model);
        self
    }

    /// Carry the Operative's stored reasoning-effort tier (builder style).
    /// Empty / whitespace-only normalizes to `None`.
    pub fn with_reasoning_effort(mut self, effort: Option<String>) -> Self {
        self.reasoning_effort = normalize_pref(effort);
        self
    }

    /// Carry a resumable adapter session id (builder style). Empty /
    /// whitespace-only normalizes to `None`; the stricter argv-injection
    /// validation (no whitespace/control, no leading `-`) is applied at
    /// argv-construction time in [`argv_with_resume`], mirroring how
    /// `model_preference` is normalized here but flag-cleaned in
    /// [`model_flag_args`].
    pub fn with_resume_session_id(mut self, session_id: Option<String>) -> Self {
        self.resume_session_id = normalize_pref(session_id);
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

/// One transcript event from a run — the focused "what happened" record
/// the dashboard shows when an operator clicks a run. Already
/// secret-redacted + length-bounded by the Rig before it reaches the
/// store; the store applies the per-run event-count cap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RigEvent {
    /// e.g. `assistant_message`, `tool_use`, `command`, `file_change`,
    /// `permission_denied`, `error`, `result`, `usage`, `output`.
    pub kind: String,
    /// Which side produced it: `claude` / `codex` / `echo` (the Rig).
    pub source: String,
    /// Short human-readable line (bounded).
    pub message: String,
    /// Optional compact structured detail (bounded JSON). Never raw JSONL.
    pub payload_json: Option<String>,
}

impl RigEvent {
    pub fn new(
        kind: impl Into<String>,
        source: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind: kind.into(),
            source: source.into(),
            message: message.into(),
            payload_json: None,
        }
    }
    pub fn with_payload(mut self, payload: impl Into<String>) -> Self {
        self.payload_json = Some(payload.into());
        self
    }
}

/// A run's outcome PLUS its transcript events — returned by
/// [`Rig::run_transcript`].
#[derive(Clone, Debug)]
pub struct RigRun {
    pub outcome: RigOutcome,
    pub events: Vec<RigEvent>,
    /// Usage / cost / session parsed from the adapter's structured output.
    /// Empty for the default (echo / raw) path.
    pub usage: RunUsage,
}

/// Process-global registry of in-flight, cancellable runs. A run
/// registers its `run_id` before spawning; a `ProcessRig` polls the flag
/// (lock-free `AtomicBool`) in its wait loop and kills the child when it
/// flips; the cancel endpoint flips it. Keyed by `run_id` so no
/// non-`Eq` handle has to live inside [`RigRunRequest`].
#[derive(Default)]
pub struct CancelRegistry {
    map: std::sync::Mutex<
        std::collections::HashMap<String, std::sync::Arc<std::sync::atomic::AtomicBool>>,
    >,
}

impl CancelRegistry {
    pub fn global() -> &'static CancelRegistry {
        static REG: std::sync::OnceLock<CancelRegistry> = std::sync::OnceLock::new();
        REG.get_or_init(CancelRegistry::default)
    }

    /// Register a run as cancellable; returns its (cleared) flag handle.
    pub fn register(&self, run_id: &str) {
        if run_id.is_empty() {
            return;
        }
        if let Ok(mut m) = self.map.lock() {
            m.insert(
                run_id.to_string(),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            );
        }
    }

    /// The lock-free flag for `run_id`, if it is a live cancellable run.
    pub fn handle(&self, run_id: &str) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        self.map.lock().ok()?.get(run_id).cloned()
    }

    /// Request cancellation — flips the flag. Returns true when the run was
    /// live (registered); false when it is unknown / already finished.
    pub fn request(&self, run_id: &str) -> bool {
        match self.map.lock().ok().and_then(|m| m.get(run_id).cloned()) {
            Some(flag) => {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                true
            }
            None => false,
        }
    }

    /// Whether cancellation was requested for `run_id`.
    pub fn is_cancelled(&self, run_id: &str) -> bool {
        self.map
            .lock()
            .ok()
            .and_then(|m| {
                m.get(run_id)
                    .map(|f| f.load(std::sync::atomic::Ordering::SeqCst))
            })
            .unwrap_or(false)
    }

    /// Drop a finished run's entry.
    pub fn clear(&self, run_id: &str) {
        if let Ok(mut m) = self.map.lock() {
            m.remove(run_id);
        }
    }

    /// The run_ids currently registered as in-flight (a live child process is
    /// being tracked). Used by the stale-run recovery sweep as the "genuinely
    /// live" set so a real, long-running Shift is never mistaken for stale.
    pub fn live_ids(&self) -> std::collections::HashSet<String> {
        self.map
            .lock()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }
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
    ("probe_failed", format!("readiness probe failed: {detail}"))
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

    /// Run one Brief AND return its transcript events (the "what
    /// happened" record). The default wraps [`Rig::run`] with no events
    /// (raw adapters rely on the coordinator's lifecycle events);
    /// `ProcessRig` overrides it to parse adapter JSONL into events.
    fn run_transcript(&self, req: &RigRunRequest) -> RigRun {
        RigRun {
            outcome: self.run(req),
            events: Vec::new(),
            usage: RunUsage::default(),
        }
    }
}

/// The canonical names of the Rigs Relix ships out of the box: the
/// safe-local built-in [`EchoRig`] plus the standard subscription CLI
/// Rigs ([`register_cli_rigs`]). This is the **narrow allowlist** a
/// governed onboarding point (e.g. `agent.approve_hire`) validates an
/// explicitly-requested Rig against, without needing a live
/// [`RigRegistry`] in hand. `echo` is the only safe-local entry; the
/// rest spawn external CLIs and are an operator's explicit, non-default
/// choice. Kept in sync with the builtins + [`register_cli_rigs`] by
/// `known_rig_names_match_the_registry` below.
pub const KNOWN_RIG_NAMES: &[&str] = &["echo", "claude", "codex", "gemini", "hermes"];

/// `echo` — the only Rig that runs **safe, local, no-network** work and
/// is therefore the one a first-run on-ramp may suggest/assign.
pub const SAFE_LOCAL_RIG: &str = "echo";

/// Whether `name` is one of the [`KNOWN_RIG_NAMES`] Relix ships. Used to
/// reject a typo'd / unknown Rig at a governed assignment point before it
/// is stored (the dispatcher would otherwise silently fall back to the
/// Guild default for an unknown name).
pub fn is_known_rig(name: &str) -> bool {
    KNOWN_RIG_NAMES.contains(&name.trim())
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
    /// Parse Codex CLI `exec --json` JSONL: extract the LAST
    /// `item.completed` of `item.type == "agent_message"` (the model's
    /// final answer) as the summary, and map an `error` / `turn.failed` /
    /// `thread.error` event to a failure.
    CodexJsonl,
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

/// The fields Relix reads from a Codex `exec --json` (JSONL) stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexRunResult {
    /// The model's final answer — the text of the LAST `item.completed`
    /// whose `item.type == "agent_message"`.
    pub text: String,
    /// A run-level error message, if Codex reported one (`error` /
    /// `turn.failed` / `thread.error` event, or an `error` item). `None`
    /// on a clean run.
    pub error: Option<String>,
    /// Whether a terminal event (`turn.completed` / `turn.failed`) was
    /// seen — i.e. the JSONL stream really came from Codex exec.
    pub saw_terminal: bool,
}

/// Parse Codex CLI `exec --json` (JSONL) stdout. The stream is
/// `thread.started` → `turn.started` → one or more `item.completed`
/// (reasoning / command_execution / file_change / **agent_message**) →
/// `turn.completed`. We take the LAST `agent_message` item's text as the
/// answer and surface any error/failure event. Returns `None` only when
/// the output carries no recognizable Codex event (so the caller falls
/// back to exit-code handling). Pure + line-driven → unit-testable.
pub fn parse_codex_jsonl(stdout: &str) -> Option<CodexRunResult> {
    let mut result = CodexRunResult::default();
    let mut saw_any = false;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(ty) = v.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        saw_any = true;
        match ty {
            "item.completed" | "item.updated" => {
                let item = v.get("item");
                let item_ty = item
                    .and_then(|i| i.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if item_ty == "agent_message" {
                    if let Some(t) = item.and_then(|i| i.get("text")).and_then(|t| t.as_str()) {
                        result.text = t.to_string();
                    }
                } else if item_ty == "error" {
                    let msg = item
                        .and_then(|i| i.get("message").or_else(|| i.get("text")))
                        .and_then(|m| m.as_str())
                        .unwrap_or("codex item error");
                    result.error = Some(msg.to_string());
                }
            }
            "error" | "thread.error" | "turn.failed" => {
                // Pull a human message from common shapes.
                let msg = v
                    .get("message")
                    .or_else(|| v.get("error").and_then(|e| e.get("message")))
                    .or_else(|| v.get("error"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("codex run failed")
                    .to_string();
                result.error = Some(msg);
                result.saw_terminal = true;
            }
            "turn.completed" => {
                result.saw_terminal = true;
            }
            _ => {}
        }
    }
    if saw_any || result.saw_terminal {
        Some(result)
    } else {
        None
    }
}

/// Usage / cost / session captured from a CLI adapter's structured output.
/// EVERY field is optional — absent or unparseable data stays `None` and is
/// stored as NULL on the run ledger (never faked).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunUsage {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    /// Cost in micro-USD (1e-6 USD), derived from the adapter's reported
    /// cost when present.
    pub cost_micros: Option<i64>,
    pub session_id: Option<String>,
}

impl RunUsage {
    /// True when nothing was captured (no column should be written).
    pub fn is_empty(&self) -> bool {
        self.provider.is_none()
            && self.model.is_none()
            && self.input_tokens.is_none()
            && self.output_tokens.is_none()
            && self.cached_input_tokens.is_none()
            && self.cost_micros.is_none()
            && self.session_id.is_none()
    }
}

/// Extract usage/cost/model/session from Claude Code `stream-json` stdout:
/// the terminal `result` event's `usage` + `total_cost_usd` + `session_id`,
/// plus the model from a `system`/assistant event. Robust to malformed lines
/// (skipped) — returns an empty [`RunUsage`] when nothing parses (never
/// panics, never fakes a value).
pub fn parse_claude_usage(stdout: &str) -> RunUsage {
    let mut u = RunUsage::default();
    let mut model: Option<String> = None;
    let mut saw_result = false;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if model.is_none() {
            if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
                model = Some(m.to_string());
            } else if let Some(m) = v
                .get("message")
                .and_then(|x| x.get("model"))
                .and_then(|m| m.as_str())
            {
                model = Some(m.to_string());
            }
        }
        if v.get("type").and_then(|t| t.as_str()) != Some("result") {
            continue;
        }
        saw_result = true;
        if let Some(usage) = v.get("usage") {
            u.input_tokens = usage.get("input_tokens").and_then(|x| x.as_i64());
            u.output_tokens = usage.get("output_tokens").and_then(|x| x.as_i64());
            u.cached_input_tokens = usage
                .get("cache_read_input_tokens")
                .and_then(|x| x.as_i64());
        }
        if let Some(cost) = v.get("total_cost_usd").and_then(|c| c.as_f64()) {
            u.cost_micros = Some((cost * 1_000_000.0).round() as i64);
        }
        if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
            u.session_id = Some(sid.to_string());
        }
    }
    if saw_result {
        u.provider = Some("anthropic".to_string());
        u.model = model;
    }
    u
}

/// Extract usage/model/session from Codex `exec --json` (JSONL) stdout:
/// `turn.completed.usage` (input/output/cached tokens) + the resumable
/// thread id (`thread.started`). Codex does not emit a per-run cost, so
/// `cost_micros` stays `None`. Robust to malformed lines; never panics.
pub fn parse_codex_usage(stdout: &str) -> RunUsage {
    let mut u = RunUsage::default();
    let mut saw_codex = false;
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if matches!(ty, "thread.started" | "turn.started" | "turn.completed")
            || ty.starts_with("item.")
        {
            saw_codex = true;
        }
        if u.session_id.is_none() {
            if let Some(tid) = v.get("thread_id").and_then(|t| t.as_str()) {
                u.session_id = Some(tid.to_string());
            } else if let Some(tid) = v
                .get("thread")
                .and_then(|t| t.get("id"))
                .and_then(|t| t.as_str())
            {
                u.session_id = Some(tid.to_string());
            }
        }
        if u.model.is_none()
            && let Some(m) = v.get("model").and_then(|m| m.as_str())
        {
            u.model = Some(m.to_string());
        }
        if ty == "turn.completed"
            && let Some(usage) = v.get("usage")
        {
            u.input_tokens = usage.get("input_tokens").and_then(|x| x.as_i64());
            u.output_tokens = usage.get("output_tokens").and_then(|x| x.as_i64());
            u.cached_input_tokens = usage.get("cached_input_tokens").and_then(|x| x.as_i64());
        }
    }
    if saw_codex {
        u.provider = Some("openai".to_string());
    } else {
        u = RunUsage::default();
    }
    u
}

/// Max bytes for a single transcript-event message / payload before it is
/// truncated. Keeps the per-run transcript bounded — a chatty agent can't
/// flood the ledger.
pub const MAX_EVENT_MESSAGE_BYTES: usize = 2048;
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 4096;

/// Truncate a string to `max` bytes on a char boundary, appending a clear
/// marker when it was cut.
fn bounded(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} …[truncated]", &s[..end])
}

fn bounded_line(s: &str) -> String {
    bounded(s.trim(), MAX_EVENT_MESSAGE_BYTES)
}

/// Extract focused transcript events from Claude `stream-json` JSONL.
/// Captures assistant text, tool-use, the final result, permission
/// denials, usage/cost, and errors — never the raw JSONL, and every text
/// field is secret-redacted + length-bounded.
pub fn claude_events(stdout: &str, bridge_token: &str) -> Vec<RigEvent> {
    let red = |s: &str| bounded_line(&redact_secrets(s, bridge_token));
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                if let Some(content) = v
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_array())
                {
                    for block in content {
                        match block.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(t) = block.get("text").and_then(|t| t.as_str())
                                    && !t.trim().is_empty()
                                {
                                    out.push(RigEvent::new("assistant_message", "claude", red(t)));
                                }
                            }
                            Some("tool_use") => {
                                let name =
                                    block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                                let input = block
                                    .get("input")
                                    .map(|i| bounded(&i.to_string(), MAX_EVENT_PAYLOAD_BYTES));
                                let mut ev =
                                    RigEvent::new("tool_use", "claude", format!("tool: {name}"));
                                if let Some(p) = input {
                                    ev = ev.with_payload(redact_secrets(&p, bridge_token));
                                }
                                out.push(ev);
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("result") => {
                if let Some(t) = v.get("result").and_then(|r| r.as_str())
                    && !t.trim().is_empty()
                {
                    out.push(RigEvent::new("result", "claude", red(t)));
                }
                let denials = v
                    .get("permission_denials")
                    .and_then(|d| d.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                if denials > 0 {
                    out.push(RigEvent::new(
                        "permission_denied",
                        "claude",
                        format!("{denials} tool permission(s) denied (noninteractive run)"),
                    ));
                }
                if let Some(cost) = v.get("total_cost_usd").and_then(|c| c.as_f64()) {
                    out.push(RigEvent::new("usage", "claude", format!("cost ${cost:.4}")));
                }
            }
            _ => {}
        }
    }
    out
}

/// Extract focused transcript events from Codex `exec --json` JSONL:
/// thread/turn lifecycle, agent messages, command + file-change items,
/// and errors. Never the raw JSONL; text is redacted + bounded.
pub fn codex_events(stdout: &str, bridge_token: &str) -> Vec<RigEvent> {
    let red = |s: &str| bounded_line(&redact_secrets(s, bridge_token));
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(ty) = v.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        match ty {
            "thread.started" => {
                out.push(RigEvent::new("thread_started", "codex", "thread started"))
            }
            "turn.started" => out.push(RigEvent::new("turn_started", "codex", "turn started")),
            "turn.completed" => {
                let msg = v
                    .get("usage")
                    .map(|u| format!("turn completed ({u})"))
                    .unwrap_or_else(|| "turn completed".to_string());
                out.push(RigEvent::new("turn_completed", "codex", bounded_line(&msg)));
            }
            "turn.failed" | "error" | "thread.error" => {
                let m = v
                    .get("message")
                    .or_else(|| v.get("error").and_then(|e| e.get("message")))
                    .or_else(|| v.get("error"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("codex error");
                out.push(RigEvent::new("error", "codex", red(m)));
            }
            "item.completed" | "item.started" => {
                let item = v.get("item");
                let item_ty = item
                    .and_then(|i| i.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                match item_ty {
                    "agent_message" => {
                        if let Some(t) = item.and_then(|i| i.get("text")).and_then(|t| t.as_str())
                            && !t.trim().is_empty()
                        {
                            out.push(RigEvent::new("assistant_message", "codex", red(t)));
                        }
                    }
                    "command_execution" => {
                        let cmd = item
                            .and_then(|i| i.get("command"))
                            .and_then(|c| c.as_str())
                            .unwrap_or("command");
                        out.push(RigEvent::new("command", "codex", red(cmd)));
                    }
                    "file_change" => {
                        let path = item
                            .and_then(|i| i.get("path").or_else(|| i.get("file")))
                            .and_then(|p| p.as_str())
                            .unwrap_or("file");
                        out.push(RigEvent::new("file_change", "codex", red(path)));
                    }
                    "error" => {
                        let m = item
                            .and_then(|i| i.get("message").or_else(|| i.get("text")))
                            .and_then(|m| m.as_str())
                            .unwrap_or("codex item error");
                        out.push(RigEvent::new("error", "codex", red(m)));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

/// Build the per-run model/effort flag args a CLI adapter accepts, keyed
/// on its [`RigOutputFormat`] — the safe discriminator between the Claude
/// and Codex subscription adapters (relix-agent-adapters.md §3.2/§3.3).
/// Returns an empty vec for `Raw` (echo / Gemini / generic process) or
/// when no usable preference is set, so non-CLI adapters are untouched.
///
/// - Claude (`ClaudeStreamJson`): `--model <model>` when a model pref is
///   set. Claude Code exposes no headless reasoning-effort flag, so effort
///   is intentionally NOT mapped here.
/// - Codex (`CodexJsonl`): `--model <model>` plus, for effort,
///   `-c model_reasoning_effort=<effort>` (matching the doc).
///
/// The caller passes each element as a DISCRETE argv entry (never a joined
/// shell string), so a Brief's content cannot inject. As a second layer, a
/// value that is empty or contains any whitespace/control character is
/// skipped — a malformed preference can never become a stray flag.
pub fn model_flag_args(
    format: RigOutputFormat,
    model: Option<&str>,
    effort: Option<&str>,
) -> Vec<String> {
    let clean = |v: Option<&str>| -> Option<String> {
        let v = v?.trim();
        if v.is_empty() || v.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return None;
        }
        Some(v.to_string())
    };
    let mut out = Vec::new();
    match format {
        RigOutputFormat::ClaudeStreamJson => {
            if let Some(m) = clean(model) {
                out.push("--model".to_string());
                out.push(m);
            }
        }
        RigOutputFormat::CodexJsonl => {
            if let Some(m) = clean(model) {
                out.push("--model".to_string());
                out.push(m);
            }
            if let Some(e) = clean(effort) {
                out.push("-c".to_string());
                out.push(format!("model_reasoning_effort={e}"));
            }
        }
        RigOutputFormat::Raw => {}
    }
    out
}

/// Splice `extra` argv elements into `base` so a trailing stdin marker
/// (`-`) stays LAST: Codex's `exec … -` reads the prompt from stdin via
/// that positional `-`, so injected flags MUST precede it. When `base`
/// has no trailing `-` (e.g. Claude), the extras are appended. A no-op
/// when `extra` is empty.
fn argv_with_model_flags(base: &[String], extra: Vec<String>) -> Vec<String> {
    if extra.is_empty() {
        return base.to_vec();
    }
    let mut out: Vec<String> = base.to_vec();
    if out.last().map(|s| s == "-").unwrap_or(false) {
        let pos = out.len() - 1;
        for (i, e) in extra.into_iter().enumerate() {
            out.insert(pos + i, e);
        }
    } else {
        out.extend(extra);
    }
    out
}

/// Validate a stored adapter session id before it can become a discrete argv
/// element. The id is **adapter state, not user input**, but it is still
/// validated defensively: trims, then rejects an empty value, any
/// whitespace/control character, or a leading `-` (which a CLI could parse as
/// a flag). Returns the safe owned value, or `None` (→ run fresh) — a
/// malformed id can never become a stray flag or a spawn of malformed argv.
fn clean_session_id(session_id: Option<&str>) -> Option<String> {
    let v = session_id?.trim();
    if v.is_empty() || v.starts_with('-') || v.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    Some(v.to_string())
}

/// Build the argv that **continues a prior adapter session**, keyed on the
/// adapter's [`RigOutputFormat`] — the same safe discriminator
/// [`model_flag_args`] uses (relix-agent-adapters.md §3.3).
///
/// - **Codex (`CodexJsonl`)** — maps resume:
///   `codex exec resume [OPTIONS] <session> -`. The `resume` subcommand is
///   spliced in right after the leading `exec`; existing options stay before
///   the session id, and the trailing stdin `-` marker stays last (so the
///   prompt still reads from stdin). Defensive: it only transforms a
///   recognizably-Codex argv that begins with `exec`.
/// - **Claude (`ClaudeStreamJson`)** — deliberately **NOT** mapped. Claude
///   Code's `--print --resume <session>` resolves the session from the run's
///   working directory, and Relix runs every Shift in a FRESH per-run scoped
///   workspace, so a resumed session would not reliably resolve. Until a
///   stable per-line-of-work workspace exists for Claude, resume stays Codex-
///   only (documented in `docs/current-limitations.md`).
/// - **Raw / echo / Gemini / generic** — ignore resume entirely.
///
/// Returns `base` unchanged whenever resume does not apply (unsupported
/// format, no/blank/invalid session id, or a Codex argv missing its `exec`
/// subcommand). The session id is passed as a DISCRETE argv element (never a
/// joined shell string), and a value that fails [`clean_session_id`] is
/// skipped rather than spawned.
pub fn argv_with_resume(
    base: &[String],
    format: RigOutputFormat,
    session_id: Option<&str>,
) -> Vec<String> {
    let Some(sid) = clean_session_id(session_id) else {
        return base.to_vec();
    };
    match format {
        RigOutputFormat::CodexJsonl => {
            // Only transform a recognizably-Codex `exec …` argv; anything else
            // is left untouched (never inject `resume` into an unknown shape).
            if base.first().map(String::as_str) != Some("exec") {
                return base.to_vec();
            }
            let mut out: Vec<String> = Vec::with_capacity(base.len() + 2);
            out.push(base[0].clone()); // `exec`
            out.push("resume".to_string());
            if base.last().map(String::as_str) == Some("-") {
                out.extend(base[1..base.len() - 1].iter().cloned());
                out.push(sid);
                out.push("-".to_string());
            } else {
                out.extend(base[1..].iter().cloned());
                out.push(sid);
            }
            out
        }
        // Claude resume is intentionally unmapped (see the doc comment above);
        // raw / echo / Gemini / generic ignore resume.
        RigOutputFormat::ClaudeStreamJson | RigOutputFormat::Raw => base.to_vec(),
    }
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
    pub fn with_readiness(
        mut self,
        probe_args: Vec<String>,
        login_hint: impl Into<String>,
    ) -> Self {
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
        self.execute(req).0
    }

    fn run_transcript(&self, req: &RigRunRequest) -> RigRun {
        let (outcome, events, usage) = self.execute(req);
        RigRun {
            outcome,
            events,
            usage,
        }
    }
}

impl ProcessRig {
    /// The shared run body: spawn the child, stream + cap + redact its
    /// output, poll the [`CancelRegistry`] (kill on cancel), and — for CLI
    /// adapters — parse the JSONL into BOTH a clean outcome AND transcript
    /// events. `run` / `run_transcript` are thin wrappers over this.
    fn execute(&self, req: &RigRunRequest) -> (RigOutcome, Vec<RigEvent>, RunUsage) {
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
            return (
                RigOutcome::Failed {
                    reason: format!("working dir does not exist: {}", dir.display()),
                    retryable: false,
                },
                Vec::new(),
                RunUsage::default(),
            );
        }

        // Resolve the program to a spawnable (PATH+PATHEXT, npm-shim
        // fallback, `.cmd`/`.bat` → `cmd.exe /C`). A non-resolvable
        // program is a clear, non-retryable failure (it isn't installed).
        let Some(spawn) = resolve_program(&self.program, &self.fallback_paths) else {
            return (
                RigOutcome::Failed {
                    reason: format!("{} not found on PATH", self.program),
                    retryable: false,
                },
                Vec::new(),
                RunUsage::default(),
            );
        };
        // First splice in model/effort flags, then continue a prior adapter
        // session when one is carried and the adapter supports it. For Codex
        // this yields the canonical `exec resume [OPTIONS] <session> -` shape:
        // options (including `--model` / `-c model_reasoning_effort`) stay
        // before the session id, and the trailing stdin `-` stays last.
        // Claude is intentionally unmapped; raw/echo ignore. Discrete argv
        // only throughout.
        let args_with_model = argv_with_model_flags(
            &self.args,
            model_flag_args(
                self.output_format,
                req.model_preference.as_deref(),
                req.reasoning_effort.as_deref(),
            ),
        );
        let effective_args = argv_with_resume(
            &args_with_model,
            self.output_format,
            req.resume_session_id.as_deref(),
        );
        let mut command = command_for(&spawn, &effective_args);
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
                return (
                    RigOutcome::Failed {
                        reason: format!("spawn {}: {e}", self.program),
                        retryable: true,
                    },
                    Vec::new(),
                    RunUsage::default(),
                );
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

        // Wait with a hard deadline, ALSO polling the cancel flag each
        // tick. On the deadline or a cancel request, KILL the child.
        let cancel = CancelRegistry::global().handle(&req.run_id);
        enum End {
            Exited(std::process::ExitStatus),
            TimedOut,
            Cancelled,
        }
        let deadline = std::time::Instant::now() + self.timeout;
        let end = loop {
            if cancel
                .as_ref()
                .map(|c| c.load(std::sync::atomic::Ordering::SeqCst))
                .unwrap_or(false)
            {
                let _ = child.kill();
                let _ = child.wait();
                break End::Cancelled;
            }
            match child.try_wait() {
                Ok(Some(status)) => break End::Exited(status),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break End::TimedOut;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => {
                    let _ = child.kill();
                    return (
                        RigOutcome::Failed {
                            reason: format!("wait {}: {e}", self.program),
                            retryable: true,
                        },
                        Vec::new(),
                        RunUsage::default(),
                    );
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
        // Build the transcript events from the captured output (already
        // bounded + redacted inside the extractors).
        let events = self.transcript_events(&raw_stdout, &stdout, &stderr, &req.bridge_token);
        // Usage/cost/session from the structured output (empty for raw/echo
        // or when the adapter emitted nothing parseable). Captured even on a
        // cancel/timeout/non-zero exit — a partial run still consumed tokens.
        let usage = match self.output_format {
            RigOutputFormat::ClaudeStreamJson => parse_claude_usage(&raw_stdout),
            RigOutputFormat::CodexJsonl => parse_codex_usage(&raw_stdout),
            RigOutputFormat::Raw => RunUsage::default(),
        };

        match end {
            End::Cancelled => (
                RigOutcome::Failed {
                    reason: "run cancelled by operator".to_string(),
                    retryable: false,
                },
                events,
                usage,
            ),
            End::TimedOut => (
                RigOutcome::Failed {
                    reason: format!("timed out after {}s (killed)", self.timeout.as_secs()),
                    retryable: true,
                },
                events,
                usage,
            ),
            End::Exited(status) => {
                // Claude's terminal `result` event is authoritative — it
                // can exit 0 with `is_error`, or non-zero while still
                // carrying a usable result — so parse it FIRST (off the
                // raw, pre-redaction stdout so the JSON stays valid).
                if matches!(self.output_format, RigOutputFormat::ClaudeStreamJson)
                    && let Some(outcome) = self.claude_outcome(&raw_stdout, &req.bridge_token)
                {
                    return (outcome, events, usage);
                }
                if matches!(self.output_format, RigOutputFormat::CodexJsonl)
                    && let Some(outcome) = self.codex_outcome(&raw_stdout, &req.bridge_token)
                {
                    return (outcome, events, usage);
                }
                if status.success() {
                    (RigOutcome::Done { summary: stdout }, events, usage)
                } else {
                    let code = status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".to_string());
                    let detail = if stderr.is_empty() { stdout } else { stderr };
                    (
                        RigOutcome::Failed {
                            reason: format!("exit {code}: {detail}"),
                            retryable: true,
                        },
                        events,
                        usage,
                    )
                }
            }
        }
    }

    /// Turn captured output into a bounded transcript: parsed adapter
    /// events for Claude/Codex, or a compact stdout/stderr summary for a
    /// raw adapter. Adapter events come pre-bounded + redacted; the raw
    /// summary is redacted+capped here.
    fn transcript_events(
        &self,
        raw_stdout: &str,
        capped_stdout: &str,
        capped_stderr: &str,
        bridge_token: &str,
    ) -> Vec<RigEvent> {
        match self.output_format {
            RigOutputFormat::ClaudeStreamJson => claude_events(raw_stdout, bridge_token),
            RigOutputFormat::CodexJsonl => codex_events(raw_stdout, bridge_token),
            RigOutputFormat::Raw => {
                let mut ev = Vec::new();
                if !capped_stdout.is_empty() {
                    ev.push(RigEvent::new(
                        "output",
                        self.name.clone(),
                        bounded_line(capped_stdout),
                    ));
                }
                if !capped_stderr.is_empty() {
                    ev.push(RigEvent::new(
                        "stderr",
                        self.name.clone(),
                        bounded_line(capped_stderr),
                    ));
                }
                ev
            }
        }
    }

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

    /// Map Codex `exec --json` JSONL stdout to a [`RigOutcome`]. Parses
    /// the stream (off the raw, pre-redaction stdout so the JSON parses),
    /// then redacts + caps only the extracted answer:
    /// - an error / `turn.failed` event → a clear, non-retryable failure.
    /// - otherwise → `Done` with the model's final `agent_message`; when
    ///   Codex produced no message, a short honest note. Sandbox-blocked
    ///   commands surface inside Codex's own final message.
    ///
    /// Returns `None` when the output has no recognizable Codex event, so
    /// the caller falls back to exit-code handling (never fakes success).
    fn codex_outcome(&self, raw_stdout: &str, bridge_token: &str) -> Option<RigOutcome> {
        let parsed = parse_codex_jsonl(raw_stdout)?;
        if let Some(err) = parsed.error {
            let err = self.cap(redact_secrets(err.trim(), bridge_token));
            return Some(RigOutcome::Failed {
                reason: format!("codex: {err}"),
                retryable: false,
            });
        }
        let text = self.cap(redact_secrets(parsed.text.trim(), bridge_token));
        let summary = if text.is_empty() {
            "codex completed with no agent message".to_string()
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
            if (up.contains("KEY")
                || up.contains("TOKEN")
                || up.contains("SECRET")
                || up.contains("PASSWORD"))
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
    let is_word =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '=' | '.' | '/' | '+');
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
// subscription instead of metered API. Availability + login probing
// is implemented (see `ProcessRig::probe`), and Claude's `stream-json`
// AND Codex's `exec --json` JSONL are parsed into clean results
// (`RigOutputFormat::ClaudeStreamJson` / `CodexJsonl`); Gemini still
// returns raw stdout.

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
    .with_install_hint(
        "install Claude Code (npm i -g @anthropic-ai/claude-code), then `claude auth login`",
    )
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

/// Codex on a ChatGPT / Codex subscription. Runs **noninteractively** via
/// `codex exec` with the Brief prompt on **stdin** (trailing `-`):
///
/// - `--json` → JSONL events on stdout (parsed by `parse_codex_jsonl`).
/// - `--sandbox workspace-write` → the model's shell commands may write
///   ONLY within the working directory (Relix pins that to the scoped run
///   workspace), so real coding work happens **confined to the sandbox**,
///   never the repo. (Verified live: writes stayed inside the run dir.)
/// - `--skip-git-repo-check` → a scoped workspace is not a git repo;
///   without this Codex can refuse to run.
///
/// Readiness is two-step: `codex --version` (installed) then
/// `codex login status` (auth — "Logged in using ChatGPT" → available;
/// "Not logged in" → `not_authenticated`).
pub fn codex_rig() -> ProcessRig {
    ProcessRig::new(
        "codex",
        "codex",
        vec![
            "exec".to_string(),
            "--json".to_string(),
            "--sandbox".to_string(),
            "workspace-write".to_string(),
            "--skip-git-repo-check".to_string(),
            "-".to_string(),
        ],
    )
    .with_structured_output(true)
    .with_output_format(RigOutputFormat::CodexJsonl)
    .with_billing(RigBilling::subscription("openai", "5h/weekly/credits"))
    .with_install_hint("install Codex CLI, then run `codex login`")
    .with_readiness(
        vec!["--version".to_string()],
        "run `codex login` to authenticate (check with `codex login status`)",
    )
    .with_auth_probe(vec!["login".to_string(), "status".to_string()])
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
    fn known_rig_names_match_the_registry() {
        // The narrow allowlist `KNOWN_RIG_NAMES` (used to validate a Rig at a
        // governed assignment point without a live registry) must stay in lock-
        // step with the actual builtins + CLI subscription Rigs, or the gate
        // would reject a Rig the dispatcher can really run (or vice-versa).
        let mut reg = RigRegistry::with_builtins();
        register_cli_rigs(&mut reg);
        let mut shipped = reg.names();
        shipped.sort();
        let mut allowlist: Vec<String> = KNOWN_RIG_NAMES.iter().map(|s| s.to_string()).collect();
        allowlist.sort();
        assert_eq!(
            shipped, allowlist,
            "KNOWN_RIG_NAMES drifted from the registry"
        );
        assert!(is_known_rig("echo"));
        assert!(is_known_rig("  echo  "), "trims before checking");
        assert!(!is_known_rig("bogus"));
        assert!(!is_known_rig(""));
        assert_eq!(SAFE_LOCAL_RIG, "echo");
    }

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
                assert!(
                    !summary.contains("brt_secret123long_enough"),
                    "token leaked: {summary:?}"
                );
                assert!(
                    summary.contains("***"),
                    "token should be redacted: {summary:?}"
                );
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
            (
                "cmd".into(),
                vec!["/C".into(), format!("ping -n {} 127.0.0.1 >NUL", secs + 1)],
            )
        } else {
            ("sh".into(), vec!["-c".into(), format!("sleep {secs}")])
        }
    }

    #[test]
    fn process_rig_times_out_and_kills_the_child() {
        // A child that sleeps far longer than the timeout must be killed
        // and reported as a retryable timeout — not hang the worker.
        let (prog, args) = sleep_cmd(30);
        let rig =
            ProcessRig::new("slow", prog, args).with_timeout(std::time::Duration::from_millis(400));
        let started = std::time::Instant::now();
        let outcome = rig.run(&RigRunRequest::new("b", "a", "g", "x"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "should not hang"
        );
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
                assert!(
                    summary.contains(&leaf),
                    "cwd {summary:?} should contain {leaf}"
                );
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
            assert!(
                summary.contains("rm -rf"),
                "literal text preserved: {summary:?}"
            );
        }
    }

    #[test]
    fn redact_secrets_masks_tokens_and_preserves_formatting() {
        let bt = "deadbeefdeadbeef00000000";
        // Assemble the fake secret-shaped token at runtime so no full
        // provider-key-shaped literal sits in source (avoids GitHub
        // secret-scanning flagging a fake fixture).
        let fake_key = ["sk", "-ABCDEFGHIJKLMNOPQRSTUV"].concat();
        let input = format!(
            "ok line\nbridge=deadbeefdeadbeef00000000\nkey {fake_key}\nplain word\nOPENAI_API_KEY=supersecretvalue\n"
        );
        let out = redact_secrets(&input, bt);
        assert!(!out.contains(bt), "bridge token leaked: {out}");
        assert!(!out.contains(&fake_key), "sk- token leaked: {out}");
        assert!(
            !out.contains("supersecretvalue"),
            "env secret leaked: {out}"
        );
        assert!(
            out.contains("OPENAI_API_KEY=***"),
            "env name kept + masked: {out}"
        );
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

    // ── Per-run model preference → adapter flags ──────────────

    #[test]
    fn run_request_normalizes_model_and_effort_prefs() {
        // Set values are carried; empty / whitespace-only normalize to None
        // so a blank stored preference is indistinguishable from absent.
        let req = RigRunRequest::new("b", "a", "g", "p")
            .with_model_preference(Some("  gpt-5-codex  ".to_string()))
            .with_reasoning_effort(Some("high".to_string()));
        assert_eq!(req.model_preference.as_deref(), Some("gpt-5-codex"));
        assert_eq!(req.reasoning_effort.as_deref(), Some("high"));

        let blank = RigRunRequest::new("b", "a", "g", "p")
            .with_model_preference(Some("   ".to_string()))
            .with_reasoning_effort(Some(String::new()));
        assert_eq!(blank.model_preference, None);
        assert_eq!(blank.reasoning_effort, None);

        // A fresh request carries neither (backward-compatible default).
        let bare = RigRunRequest::new("b", "a", "g", "p");
        assert_eq!(bare.model_preference, None);
        assert_eq!(bare.reasoning_effort, None);
    }

    #[test]
    fn model_flag_args_maps_per_format() {
        // Claude: only `--model` (no headless effort flag).
        assert_eq!(
            model_flag_args(
                RigOutputFormat::ClaudeStreamJson,
                Some("claude-sonnet-4"),
                Some("high")
            ),
            vec!["--model".to_string(), "claude-sonnet-4".to_string()]
        );
        // Codex: `--model` AND `-c model_reasoning_effort=<effort>`.
        assert_eq!(
            model_flag_args(
                RigOutputFormat::CodexJsonl,
                Some("gpt-5-codex"),
                Some("medium")
            ),
            vec![
                "--model".to_string(),
                "gpt-5-codex".to_string(),
                "-c".to_string(),
                "model_reasoning_effort=medium".to_string(),
            ]
        );
        // Codex effort alone (no model) still maps the effort.
        assert_eq!(
            model_flag_args(RigOutputFormat::CodexJsonl, None, Some("low")),
            vec!["-c".to_string(), "model_reasoning_effort=low".to_string()]
        );
        // Raw / echo / generic: never mapped.
        assert!(model_flag_args(RigOutputFormat::Raw, Some("x"), Some("high")).is_empty());
        // Absent prefs → no flags on any format.
        assert!(model_flag_args(RigOutputFormat::ClaudeStreamJson, None, None).is_empty());
        assert!(model_flag_args(RigOutputFormat::CodexJsonl, None, None).is_empty());
    }

    #[test]
    fn model_flag_args_skips_malformed_values() {
        // Whitespace / control chars inside a value → skipped, never a stray
        // flag or argument (defense in depth atop the store's write-time guard).
        assert!(model_flag_args(RigOutputFormat::ClaudeStreamJson, Some("a b"), None).is_empty());
        assert!(model_flag_args(RigOutputFormat::CodexJsonl, Some("m\tx"), None).is_empty());
        assert!(model_flag_args(RigOutputFormat::CodexJsonl, None, Some("hi gh")).is_empty());
        assert!(model_flag_args(RigOutputFormat::ClaudeStreamJson, Some("   "), None).is_empty());
    }

    #[test]
    fn claude_argv_appends_model_flag_safely() {
        // The EXACT argv `ProcessRig::execute` builds for the Claude rig with a
        // model preference: `--model <m>` appended (no trailing stdin marker).
        let base = claude_rig().args().to_vec();
        let argv = argv_with_model_flags(
            &base,
            model_flag_args(
                RigOutputFormat::ClaudeStreamJson,
                Some("claude-sonnet-4"),
                None,
            ),
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--model" && w[1] == "claude-sonnet-4")
        );
        // The original flags are preserved.
        assert!(argv.iter().any(|a| a == "--print"));
        assert!(argv.iter().any(|a| a == "stream-json"));
    }

    #[test]
    fn codex_argv_inserts_flags_before_trailing_stdin_marker() {
        // Codex's `exec … -` reads the prompt from stdin via the positional
        // `-`, which MUST stay last; the model/effort flags are spliced in
        // just before it.
        let base = codex_rig().args().to_vec();
        assert_eq!(
            base.last().map(String::as_str),
            Some("-"),
            "codex base ends with stdin marker"
        );
        let argv = argv_with_model_flags(
            &base,
            model_flag_args(
                RigOutputFormat::CodexJsonl,
                Some("gpt-5-codex"),
                Some("high"),
            ),
        );
        assert_eq!(
            argv.last().map(String::as_str),
            Some("-"),
            "stdin marker stays last"
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--model" && w[1] == "gpt-5-codex")
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-c" && w[1] == "model_reasoning_effort=high")
        );
        // `exec` / `--json` / sandbox flags survive.
        assert!(argv.iter().any(|a| a == "exec"));
        assert!(argv.iter().any(|a| a == "--json"));
    }

    // ── Resume a prior adapter session → adapter argv ────────────

    #[test]
    fn run_request_normalizes_resume_session() {
        // A set id is carried; empty / whitespace-only normalizes to None.
        let req = RigRunRequest::new("b", "a", "g", "p")
            .with_resume_session_id(Some("  thread-abc123  ".to_string()));
        assert_eq!(req.resume_session_id.as_deref(), Some("thread-abc123"));
        let blank =
            RigRunRequest::new("b", "a", "g", "p").with_resume_session_id(Some("   ".to_string()));
        assert_eq!(blank.resume_session_id, None);
        // A fresh request carries no resume id (backward-compatible default).
        assert_eq!(
            RigRunRequest::new("b", "a", "g", "p").resume_session_id,
            None
        );
    }

    #[test]
    fn resume_argv_maps_codex_after_exec_keeping_stdin_marker() {
        // Codex maps resume to its documented shape:
        // `codex exec resume [OPTIONS] <session> -`. `resume` is spliced in
        // right after `exec`, existing options remain before the session id,
        // and the trailing stdin `-` marker stays LAST so the prompt still
        // reads from stdin.
        let base = codex_rig().args().to_vec();
        let argv = argv_with_resume(&base, RigOutputFormat::CodexJsonl, Some("thread-xyz"));
        assert_eq!(argv[0], "exec");
        assert_eq!(argv[1], "resume");
        let sid_at = argv.iter().position(|a| a == "thread-xyz").unwrap();
        let dash_at = argv.iter().rposition(|a| a == "-").unwrap();
        assert_eq!(
            sid_at + 1,
            dash_at,
            "session id immediately precedes stdin marker"
        );
        assert_eq!(
            argv.last().map(String::as_str),
            Some("-"),
            "stdin marker stays last"
        );
        // The original flags survive intact.
        assert!(argv.iter().any(|a| a == "--json"));
        assert!(argv.iter().any(|a| a == "workspace-write"));
        assert!(argv.iter().any(|a| a == "--skip-git-repo-check"));
        assert!(argv.iter().position(|a| a == "--json").unwrap() < sid_at);
    }

    #[test]
    fn resume_argv_unmapped_for_claude_and_raw() {
        // Claude resume is intentionally NOT mapped (its session store is keyed
        // by the run's working dir, which Relix re-scopes per run) — argv is
        // unchanged. Raw / echo / generic ignore resume too.
        let claude = claude_rig().args().to_vec();
        assert_eq!(
            argv_with_resume(&claude, RigOutputFormat::ClaudeStreamJson, Some("sess-1")),
            claude,
            "claude argv is untouched (resume unmapped)"
        );
        let raw = vec!["run".to_string(), "-".to_string()];
        assert_eq!(
            argv_with_resume(&raw, RigOutputFormat::Raw, Some("sess-1")),
            raw,
            "raw argv is untouched"
        );
    }

    #[test]
    fn resume_argv_skips_absent_or_malformed_session() {
        let base = codex_rig().args().to_vec();
        // Absent / blank → no transformation.
        assert_eq!(
            argv_with_resume(&base, RigOutputFormat::CodexJsonl, None),
            base
        );
        assert_eq!(
            argv_with_resume(&base, RigOutputFormat::CodexJsonl, Some("   ")),
            base
        );
        // Whitespace / control chars inside the id → skipped (never a split or
        // injected argv element).
        assert_eq!(
            argv_with_resume(&base, RigOutputFormat::CodexJsonl, Some("a b")),
            base
        );
        assert_eq!(
            argv_with_resume(&base, RigOutputFormat::CodexJsonl, Some("x\ty")),
            base
        );
        // A leading `-` (flag-injection shape) is rejected even though the id
        // is adapter state, not user input.
        assert_eq!(
            argv_with_resume(&base, RigOutputFormat::CodexJsonl, Some("--dangerous")),
            base
        );
        // A non-Codex `exec …` argv shape is never transformed defensively.
        let weird = vec!["notexec".to_string(), "-".to_string()];
        assert_eq!(
            argv_with_resume(&weird, RigOutputFormat::CodexJsonl, Some("ok-id")),
            weird
        );
    }

    #[test]
    fn resume_and_model_flags_compose_for_codex() {
        // The EXACT argv `ProcessRig::execute` builds when BOTH a resume
        // session and a model/effort preference are present: resume goes after
        // `exec`, model/effort flags stay in the resume subcommand's option
        // section, the session id is the penultimate positional, and the `-`
        // stays last. The two transforms compose without disturbing each other.
        let base = codex_rig().args().to_vec();
        let with_model = argv_with_model_flags(
            &base,
            model_flag_args(
                RigOutputFormat::CodexJsonl,
                Some("gpt-5-codex"),
                Some("high"),
            ),
        );
        let argv = argv_with_resume(&with_model, RigOutputFormat::CodexJsonl, Some("thread-xyz"));
        assert_eq!(&argv[0..2], &["exec", "resume"], "resume after exec");
        assert_eq!(
            argv.last().map(String::as_str),
            Some("-"),
            "stdin marker stays last"
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "--model" && w[1] == "gpt-5-codex")
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-c" && w[1] == "model_reasoning_effort=high")
        );
        // The model flags land BEFORE the session id and stdin marker.
        let model_at = argv.iter().position(|a| a == "--model").unwrap();
        let sid_at = argv.iter().position(|a| a == "thread-xyz").unwrap();
        let dash_at = argv.iter().rposition(|a| a == "-").unwrap();
        assert!(model_at < sid_at, "model flag precedes the session id");
        assert_eq!(
            sid_at + 1,
            dash_at,
            "session id immediately precedes stdin marker"
        );
    }

    #[test]
    fn argv_with_model_flags_is_noop_without_prefs() {
        // No prefs → argv is byte-for-byte the rig's configured args (the
        // echo / default path is completely untouched).
        let base = codex_rig().args().to_vec();
        assert_eq!(argv_with_model_flags(&base, Vec::new()), base);
        let claude = claude_rig().args().to_vec();
        assert_eq!(argv_with_model_flags(&claude, Vec::new()), claude);
    }

    #[test]
    fn process_rig_run_request_with_prefs_does_not_break_echo_path() {
        // A Raw-format process rig run with model prefs present must behave
        // exactly as without them — prefs are simply ignored.
        let (prog, args) = echo_cmd("hello-with-prefs");
        let rig = ProcessRig::new("test-echo", prog, args);
        let req = RigRunRequest::new("b", "a", "g", "ignored")
            .with_model_preference(Some("gpt-5-codex".to_string()))
            .with_reasoning_effort(Some("high".to_string()));
        match rig.run(&req) {
            RigOutcome::Done { summary } => {
                assert!(summary.contains("hello-with-prefs"), "got: {summary:?}");
                // The preference text must NOT leak into raw output as a flag.
                assert!(
                    !summary.contains("--model"),
                    "raw path must not add flags: {summary:?}"
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
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
        let s = ReadinessSignals {
            missing_binary: true,
            ..Default::default()
        };
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
        let s = ReadinessSignals {
            timed_out: true,
            ..Default::default()
        };
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
            (
                "cmd".to_string(),
                vec![
                    "/C".to_string(),
                    "echo".to_string(),
                    "probe-ok 9.9".to_string(),
                ],
            )
        } else {
            (
                "sh".to_string(),
                vec!["-c".to_string(), "echo probe-ok 9.9".to_string()],
            )
        };
        let s = run_readiness_probe(&prog, &args, std::time::Duration::from_secs(5));
        assert!(
            !s.missing_binary && !s.timed_out && s.exit_ok,
            "signals: {s:?}"
        );
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
            std::slice::from_ref(&fb),
        )
        .unwrap();
        assert_eq!(
            s,
            Spawnable::Direct(fb),
            "the real .exe fallback should win over the .cmd shim"
        );
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
        assert!(
            classify_file(&noext).is_none(),
            "extensionless sh shim must not be Direct"
        );
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
        let s = resolve_in_dirs(
            "claude",
            &[path_dir.path().to_path_buf()],
            &exts,
            std::slice::from_ref(&real),
        )
        .unwrap();
        assert_eq!(
            s,
            Spawnable::Direct(real),
            "must skip the sh shim and use the real .exe"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_resolves_bare_name_to_direct() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("mytool");
        std::fs::File::create(&bin).unwrap();
        let s =
            resolve_in_dirs("mytool", &[tmp.path().to_path_buf()], &[String::new()], &[]).unwrap();
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
            sig(
                true,
                "Not logged in. Run `claude auth login` to sign in.",
                "",
            ),
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
        let auth = ReadinessSignals {
            timed_out: true,
            ..Default::default()
        };
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
            r#"{"type":"system","subtype":"hook_started"}"#, r#"{"type":"assistant"}"#,
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
                assert_eq!(
                    summary, "Relix Claude test passed",
                    "no JSONL noise in the summary"
                );
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
                assert!(
                    summary.contains("Created the note"),
                    "keeps the model reply: {summary}"
                );
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
                assert!(
                    reason.contains("error_during_execution"),
                    "reason: {reason}"
                );
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
        assert_eq!(
            claude_rig().output_format,
            RigOutputFormat::ClaudeStreamJson
        );
    }

    #[test]
    fn claude_rig_uses_two_step_readiness_and_windows_fallback() {
        let rig = claude_rig();
        let r = rig
            .readiness
            .as_ref()
            .expect("claude has a readiness check");
        assert_eq!(r.probe_args, vec!["--version".to_string()]);
        assert_eq!(
            r.auth_args.as_deref(),
            Some(
                &[
                    "auth".to_string(),
                    "status".to_string(),
                    "--text".to_string()
                ][..]
            )
        );
        assert!(
            r.login_hint.contains("claude auth login"),
            "hint: {}",
            r.login_hint
        );
        assert!(
            rig.install_hint
                .as_deref()
                .unwrap()
                .contains("claude auth login"),
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

    // ── Codex exec --json result parsing ────────────────────────

    // A representative `codex exec --json` JSONL stream: thread/turn
    // bookkeeping, an interim item, the final agent_message, turn done.
    fn codex_jsonl(agent_message: &str) -> String {
        let agent_message = serde_json::to_string(agent_message).unwrap();
        format!(
            "{}\n{}\n{}\n{}\n{}\n",
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"id":"i0","type":"reasoning","text":"thinking"}}"#,
            format_args!(
                r#"{{"type":"item.completed","item":{{"id":"i1","type":"agent_message","text":{agent_message}}}}}"#
            ),
            r#"{"type":"turn.completed","usage":{"input_tokens":26335,"output_tokens":21}}"#,
        )
    }

    #[test]
    fn parse_codex_jsonl_extracts_last_agent_message() {
        let jsonl = codex_jsonl("Relix Codex test passed");
        let r = parse_codex_jsonl(&jsonl).expect("a codex stream");
        assert_eq!(r.text, "Relix Codex test passed");
        assert!(r.error.is_none());
        assert!(r.saw_terminal, "turn.completed seen");
    }

    #[test]
    fn parse_codex_jsonl_surfaces_error_events() {
        for jsonl in [
            r#"{"type":"thread.started"}
{"type":"turn.failed","error":{"message":"model overloaded"}}"#,
            r#"{"type":"error","message":"sandbox denied write outside workspace"}"#,
            r#"{"type":"item.completed","item":{"type":"error","message":"command failed"}}"#,
        ] {
            let r = parse_codex_jsonl(jsonl).expect("codex stream");
            assert!(r.error.is_some(), "should carry an error for: {jsonl}");
        }
    }

    #[test]
    fn parse_codex_jsonl_none_without_events() {
        assert!(parse_codex_jsonl("not json\n\n{bad").is_none());
        assert!(parse_codex_jsonl("").is_none());
    }

    // ── Usage / cost / session capture (TG6) ──────────────────────

    #[test]
    fn parse_claude_usage_extracts_tokens_cost_model_session() {
        let stdout = concat!(
            r#"{"type":"system","subtype":"init","model":"claude-sonnet-4"}"#,
            "\n",
            r#"{"type":"result","is_error":false,"result":"done","session_id":"sess-abc","total_cost_usd":0.0123,"usage":{"input_tokens":100,"output_tokens":42,"cache_read_input_tokens":7}}"#,
            "\n",
        );
        let u = parse_claude_usage(stdout);
        assert_eq!(u.provider.as_deref(), Some("anthropic"));
        assert_eq!(u.model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(u.input_tokens, Some(100));
        assert_eq!(u.output_tokens, Some(42));
        assert_eq!(u.cached_input_tokens, Some(7));
        assert_eq!(u.cost_micros, Some(12_300)); // 0.0123 USD -> micros
        assert_eq!(u.session_id.as_deref(), Some("sess-abc"));
        assert!(!u.is_empty());
    }

    #[test]
    fn parse_claude_usage_none_without_result_event() {
        let u = parse_claude_usage(r#"{"type":"system","subtype":"init","model":"x"}"#);
        assert!(u.is_empty(), "no terminal result -> empty (never faked)");
    }

    #[test]
    fn parse_usage_tolerates_malformed_lines() {
        // A non-JSON line + a result with garbage usage/cost must not panic
        // and must leave the unparseable fields null.
        let stdout = concat!(
            "not json at all\n",
            r#"{"type":"result","usage":"garbage","total_cost_usd":"oops"}"#,
            "\n",
        );
        let u = parse_claude_usage(stdout);
        assert_eq!(u.provider.as_deref(), Some("anthropic")); // a result was seen
        assert!(u.input_tokens.is_none());
        assert!(u.cost_micros.is_none());
    }

    #[test]
    fn parse_codex_usage_extracts_tokens_and_session_no_cost() {
        let stdout = concat!(
            r#"{"type":"thread.started","thread_id":"th-1"}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":26335,"output_tokens":21,"cached_input_tokens":10}}"#,
            "\n",
        );
        let u = parse_codex_usage(stdout);
        assert_eq!(u.provider.as_deref(), Some("openai"));
        assert_eq!(u.input_tokens, Some(26335));
        assert_eq!(u.output_tokens, Some(21));
        assert_eq!(u.cached_input_tokens, Some(10));
        assert_eq!(u.session_id.as_deref(), Some("th-1"));
        assert!(u.cost_micros.is_none(), "codex emits no per-run cost");
    }

    #[test]
    fn parse_codex_usage_empty_for_non_codex() {
        assert!(parse_codex_usage("hello world\n{}").is_empty());
        assert!(parse_codex_usage("").is_empty());
    }

    fn codex_test_rig() -> ProcessRig {
        ProcessRig::new("codex", "codex", vec![]).with_output_format(RigOutputFormat::CodexJsonl)
    }

    #[test]
    fn codex_outcome_success_returns_clean_answer() {
        let jsonl = codex_jsonl("Created `codex-note.txt` containing `hello`.");
        match codex_test_rig().codex_outcome(&jsonl, "") {
            Some(RigOutcome::Done { summary }) => {
                assert_eq!(summary, "Created `codex-note.txt` containing `hello`.");
                assert!(!summary.contains("thread.started"), "no JSONL noise");
                assert!(!summary.contains("\"type\""), "no raw JSON in the summary");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn codex_outcome_error_is_a_clear_failure() {
        let jsonl = r#"{"type":"turn.failed","error":{"message":"model overloaded"}}"#;
        match codex_test_rig().codex_outcome(jsonl, "") {
            Some(RigOutcome::Failed { reason, retryable }) => {
                assert!(!retryable);
                assert!(reason.contains("codex:"), "reason: {reason}");
                assert!(reason.contains("model overloaded"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn codex_outcome_none_without_events_falls_through() {
        assert!(
            codex_test_rig()
                .codex_outcome("plain text, no json", "")
                .is_none()
        );
    }

    #[test]
    fn codex_rig_uses_jsonl_parser_safe_sandbox_and_auth_check() {
        let rig = codex_rig();
        assert_eq!(rig.output_format, RigOutputFormat::CodexJsonl);
        // Safe, noninteractive, confined command shape — no shell strings.
        let args = rig.args();
        assert_eq!(args.first().map(String::as_str), Some("exec"));
        assert!(args.iter().any(|a| a == "--json"));
        assert!(args.iter().any(|a| a == "--sandbox"));
        assert!(args.iter().any(|a| a == "workspace-write"));
        assert!(args.iter().any(|a| a == "--skip-git-repo-check"));
        assert_eq!(args.last().map(String::as_str), Some("-"));
        // Two-step readiness: --version + `login status`.
        let r = rig.readiness.as_ref().expect("codex has readiness");
        assert_eq!(r.probe_args, vec!["--version".to_string()]);
        assert_eq!(
            r.auth_args.as_deref(),
            Some(&["login".to_string(), "status".to_string()][..])
        );
    }

    // ── Transcript events + cancellation ────────────────────────

    #[test]
    fn claude_events_extracts_focused_transcript() {
        let jsonl = format!(
            "{}\n{}\n{}\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working on it"},{"type":"tool_use","name":"Write","input":{"path":"a.txt"}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#,
            r#"{"type":"result","result":"All set","permission_denials":[{"tool":"Bash"}],"total_cost_usd":0.0123}"#,
        );
        let ev = claude_events(&jsonl, "");
        let kinds: Vec<&str> = ev.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&"assistant_message"));
        assert!(kinds.contains(&"tool_use"));
        assert!(kinds.contains(&"result"));
        assert!(kinds.contains(&"permission_denied"));
        assert!(kinds.contains(&"usage"));
        // No raw JSON leaks into a message.
        assert!(ev.iter().all(|e| !e.message.contains("\"type\"")));
        assert!(ev.iter().all(|e| e.source == "claude"));
    }

    #[test]
    fn codex_events_extracts_lifecycle_and_items() {
        let jsonl = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            r#"{"type":"thread.started","thread_id":"t"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.completed","item":{"type":"command_execution","command":"ls -la"}}"#,
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"Listed files"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":5}}"#,
        );
        let ev = codex_events(&jsonl, "");
        let kinds: Vec<&str> = ev.iter().map(|e| e.kind.as_str()).collect();
        assert!(kinds.contains(&"thread_started"));
        assert!(kinds.contains(&"turn_started"));
        assert!(kinds.contains(&"command"));
        assert!(kinds.contains(&"assistant_message"));
        assert!(kinds.contains(&"turn_completed"));
        assert!(ev.iter().all(|e| e.source == "codex"));
    }

    #[test]
    fn transcript_events_redact_and_bound() {
        // A secret-shaped token in an assistant message is masked.
        let secret = "sk-".to_string() + &"a".repeat(40);
        let jsonl = format!(
            r#"{{"type":"result","result":{}}}"#,
            serde_json::to_string(&format!("here is the key {secret}")).unwrap()
        );
        let ev = claude_events(&jsonl, "");
        assert!(ev.iter().any(|e| e.kind == "result"));
        assert!(
            ev.iter().all(|e| !e.message.contains(&secret)),
            "secret must be redacted from events"
        );
        // Over-long text is truncated.
        let big = "x".repeat(MAX_EVENT_MESSAGE_BYTES + 500);
        let jsonl2 = format!(
            r#"{{"type":"result","result":{}}}"#,
            serde_json::to_string(&big).unwrap()
        );
        let ev2 = claude_events(&jsonl2, "");
        let msg = &ev2.iter().find(|e| e.kind == "result").unwrap().message;
        assert!(msg.len() <= MAX_EVENT_MESSAGE_BYTES + 32);
        assert!(msg.contains("truncated"));
    }

    #[test]
    fn cancel_registry_register_request_clear() {
        let reg = CancelRegistry::default();
        assert!(!reg.is_cancelled("run_x"));
        assert!(!reg.request("run_x"), "unknown run is not active");
        reg.register("run_x");
        assert!(!reg.is_cancelled("run_x"));
        assert!(reg.request("run_x"), "registered run is active");
        assert!(reg.is_cancelled("run_x"));
        reg.clear("run_x");
        assert!(!reg.is_cancelled("run_x"));
        assert!(!reg.request("run_x"), "cleared run is no longer active");
    }

    #[test]
    fn cancel_registry_request_is_idempotent() {
        // TG4: requesting cancellation twice is safe — the second request is
        // a no-op that keeps the run cancelled and still reports the live
        // handle as active. The bridge's `run.cancel` is therefore safe to
        // retry without corrupting state.
        let reg = CancelRegistry::default();
        reg.register("run_i");
        assert!(reg.request("run_i"), "first request flips + reports active");
        assert!(
            reg.request("run_i"),
            "second request is safe + still active"
        );
        assert!(reg.is_cancelled("run_i"));
        // Even after clear, a stray repeat request is harmless (inactive).
        reg.clear("run_i");
        assert!(
            !reg.request("run_i"),
            "post-clear request is inactive, not a panic"
        );
    }

    #[test]
    fn process_rig_cancel_mid_flight_kills_child_and_reports_non_success() {
        // TG4: a long-running child cancelled AFTER it starts must be killed
        // and reported as a NON-retryable `cancelled` Failed — never Done,
        // never a worker hang. Timeout is set far beyond the cancel delay so
        // we're proving the cancel path, not the deadline path.
        let run_id = "tg4-cancel-midflight";
        CancelRegistry::global().register(run_id);
        let (prog, args) = sleep_cmd(30);
        let rig =
            ProcessRig::new("slow", prog, args).with_timeout(std::time::Duration::from_secs(20));
        let req = RigRunRequest::new("b", "a", "g", "x").with_run_id(run_id);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            CancelRegistry::global().request(run_id)
        });
        let started = std::time::Instant::now();
        let outcome = rig.run(&req);
        let _ = canceller.join();
        CancelRegistry::global().clear(run_id);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "must not hang"
        );
        match outcome {
            RigOutcome::Failed { retryable, reason } => {
                assert!(!retryable, "operator cancel is non-retryable");
                assert!(reason.contains("cancelled"), "got: {reason}");
            }
            other => panic!("expected cancelled Failed, never Done; got {other:?}"),
        }
    }

    #[test]
    fn raw_rig_run_transcript_emits_output_event() {
        let (prog, args) = echo_cmd("transcript-hello");
        let rig = ProcessRig::new("echo-like", prog, args);
        let run = rig.run_transcript(&RigRunRequest::new("b", "a", "g", "x"));
        assert!(matches!(run.outcome, RigOutcome::Done { .. }));
        assert!(
            run.events
                .iter()
                .any(|e| e.kind == "output" && e.message.contains("transcript-hello")),
            "events: {:?}",
            run.events
        );
    }
}
