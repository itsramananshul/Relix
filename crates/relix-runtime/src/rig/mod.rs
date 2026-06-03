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
        }
    }

    /// Attach opaque context (builder style).
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = context.into();
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

    /// Look up a Rig by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Rig>> {
        self.rigs.get(name).cloned()
    }

    /// All registered Rig names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.rigs.keys().cloned().collect()
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
pub struct ProcessRig {
    name: String,
    program: String,
    args: Vec<String>,
}

impl ProcessRig {
    pub fn new(
        name: impl Into<String>,
        program: impl Into<String>,
        args: Vec<String>,
    ) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            args,
        }
    }

    /// The program this Rig spawns.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The arguments passed to the program.
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

impl Rig for ProcessRig {
    fn name(&self) -> &str {
        &self.name
    }

    fn run(&self, req: &RigRunRequest) -> RigOutcome {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut child = match Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                return RigOutcome::Failed {
                    reason: format!("spawn {}: {e}", self.program),
                    retryable: true,
                };
            }
        };

        // Pipe the prompt to the child, then close stdin (EOF).
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(req.prompt.as_bytes());
        }

        let output = match child.wait_with_output() {
            Ok(o) => o,
            Err(e) => {
                return RigOutcome::Failed {
                    reason: format!("wait {}: {e}", self.program),
                    retryable: true,
                };
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if output.status.success() {
            RigOutcome::Done { summary: stdout }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let code = output
                .status
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

// ── CLI subscription Rigs ─────────────────────────────────
//
// The standard CLI Rigs, as ProcessRigs. Each spawns the operator's
// installed CLI, which authenticates with ITS OWN subscription
// login — **no inference key is injected**. This is the
// subscription model from `docs/relix-agent-adapters.md`: run heavy
// agents on a flat-rate Claude Max / ChatGPT (Codex) / Gemini
// subscription instead of metered API. The flags here are the
// starting shape; future refinements add availability / login
// probing and structured-output parsing.

/// Claude Code on a Claude subscription. Prompt piped to stdin.
pub fn claude_rig() -> ProcessRig {
    ProcessRig::new("claude", "claude", vec!["--print".to_string()])
}

/// Codex on a ChatGPT / Codex subscription. Prompt piped via the
/// trailing `-` (read from stdin).
pub fn codex_rig() -> ProcessRig {
    ProcessRig::new("codex", "codex", vec!["exec".to_string(), "-".to_string()])
}

/// Gemini CLI on a Google subscription. Prompt piped to stdin.
pub fn gemini_rig() -> ProcessRig {
    ProcessRig::new("gemini", "gemini", Vec::new())
}

/// Register the standard CLI subscription Rigs into `registry`.
/// They spawn external binaries, so a Rig whose CLI isn't installed
/// simply fails gracefully at run time (a retryable `Failed`) — the
/// operator opts an Operative onto one by setting its `rig`.
pub fn register_cli_rigs(registry: &mut RigRegistry) {
    registry.register(Arc::new(claude_rig()));
    registry.register(Arc::new(codex_rig()));
    registry.register(Arc::new(gemini_rig()));
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
            RigOutcome::Failed { retryable: false, .. }
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
        assert_eq!(reg.get("echo").unwrap().governance(), RigGovernance::PerToolCall);
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
            RigOutcome::Failed { retryable: true, .. }
        ));
    }

    #[test]
    fn process_rig_maps_spawn_failure_to_retryable_failed() {
        let rig = ProcessRig::new("nope", "this-binary-does-not-exist-xyzzy", vec![]);
        let req = RigRunRequest::new("b", "a", "g", "x");
        assert!(matches!(
            rig.run(&req),
            RigOutcome::Failed { retryable: true, .. }
        ));
    }

    #[test]
    fn cli_rig_factories_configure_the_right_commands() {
        let c = claude_rig();
        assert_eq!(c.name(), "claude");
        assert_eq!(c.program(), "claude");
        assert!(c.args().iter().any(|a| a == "--print"));

        let x = codex_rig();
        assert_eq!(x.name(), "codex");
        assert_eq!(x.program(), "codex");
        assert!(x.args().iter().any(|a| a == "exec"));

        assert_eq!(gemini_rig().name(), "gemini");
    }

    #[test]
    fn register_cli_rigs_adds_them_alongside_builtins() {
        let mut reg = RigRegistry::with_builtins();
        register_cli_rigs(&mut reg);
        for name in ["echo", "claude", "codex", "gemini"] {
            assert!(reg.get(name).is_some(), "{name} should be registered");
        }
    }
}
