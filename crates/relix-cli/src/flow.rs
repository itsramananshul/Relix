//! `relix-cli flow ...` — workflow scaffolding helpers.
//!
//! `flow yaml [--template <name>]` prints a minimal YAML
//! flow template to stdout. Four templates ship:
//!
//! - `chat` (default) — single `remote_call` returning the
//!   reply. The 80% case operators reach for first.
//! - `stream` — same shape but uses `stream:` so the
//!   operator gets a streaming flow without re-reading the
//!   reference.
//! - `try` — wraps a `call` in a three-clause multi-catch
//!   (`timeout` / `policy_denied` / `any`) so the error
//!   handling pattern is right there.
//! - `loop` — counted loop calling a peer N times.
//!
//! All four templates compile through
//! `relix_runtime::yaml_flow::compile_source` without error
//! — a developer copying any of them sees a working flow on
//! their first run.

use clap::{Subcommand, ValueEnum};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print a minimal working YAML flow template to stdout.
    /// Pipe into a file (e.g. `relix flow yaml > my.yml`),
    /// edit the peer/method/arg, then run it through
    /// `relix-cli flow-run --flow my.yml ...`.
    ///
    /// `--template <name>` picks which scaffold to emit:
    /// `chat` (default), `stream`, `try`, `loop`.
    Yaml {
        /// Which template to scaffold.
        #[arg(long, value_enum, default_value_t = TemplateKind::Chat)]
        template: TemplateKind,
    },
}

/// Catalog of YAML scaffolds. Names are stable — operators
/// will script against them. Add new entries at the END so
/// existing automation keeps working.
#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum TemplateKind {
    /// Single `remote_call` returning the reply.
    Chat,
    /// Streaming variant using `stream:`.
    Stream,
    /// `try` with multi-catch error handling.
    Try,
    /// Counted loop calling a peer N times.
    Loop,
}

impl TemplateKind {
    pub fn body(self) -> &'static str {
        match self {
            TemplateKind::Chat => CHAT_TEMPLATE,
            TemplateKind::Stream => STREAM_TEMPLATE,
            TemplateKind::Try => TRY_TEMPLATE,
            TemplateKind::Loop => LOOP_TEMPLATE,
        }
    }
}

const CHAT_TEMPLATE: &str = "# Minimal Relix YAML flow — chat scaffold.
#
# Pipe into a file: relix-cli flow yaml > my.yml
# Run:               relix-cli flow-run --flow my.yml ...
# Full reference:    docs/yaml-flow-reference.md

steps:
  - let:
      name: session
      type: str
      value: \"demo-session\"
  - let:
      name: message
      type: str
      value: \"hello\"

  - call:
      peer: ai
      method: ai.chat
      arg: \"{{session}}|{{message}}|\"
      assign: reply

  - result: \"{{reply}}\"
";

const STREAM_TEMPLATE: &str = "# Relix YAML flow — streaming scaffold.
#
# `stream:` lowers to remote_call_stream — chunks flow back
# through the host's chunk observer (the bridge's SSE
# response, for example) while the VM is still running.

steps:
  - let:
      name: session
      type: str
      value: \"demo-session\"
  - let:
      name: message
      type: str
      value: \"hello\"

  - stream:
      peer: ai
      method: ai.chat.stream
      arg: \"{{session}}|{{message}}|\"
      assign: reply

  - result: \"{{reply}}\"
";

const TRY_TEMPLATE: &str = "# Relix YAML flow — error-handling scaffold.
#
# Multi-catch: timeout / policy_denied / any. First matching
# clause wins. Inside a catch, error_kind() and error_cause()
# expose the current failure.

steps:
  - let:
      name: session
      type: str
      value: \"demo-session\"
  - let:
      name: message
      type: str
      value: \"hello\"
  - let:
      name: reply
      type: str
      value: \"\"

  - try:
      steps:
        - call:
            peer: ai
            method: ai.chat
            arg: \"{{session}}|{{message}}|\"
            assign: reply
      catch:
        - kind: timeout
          steps:
            - let:
                name: reply
                type: str
                value: \"timed out, try again\"
        - kind: policy_denied
          steps:
            - let:
                name: reply
                type: str
                value: \"not allowed\"
        - kind: any
          steps:
            - let:
                name: reply
                type: str
                value: \"error\"

  - result: \"{{reply}}\"
";

const LOOP_TEMPLATE: &str = "# Relix YAML flow — counted-loop scaffold.
#
# Calls a peer N times, collecting each reply into a list.
# Adapt the body to do whatever per-iteration work you need.

steps:
  - let:
      name: results
      type: list
      value: []
  - let:
      name: reply
      type: str
      value: \"\"

  - loop:
      times: 3
      steps:
        - call:
            peer: ai
            method: ai.chat
            arg: \"demo|tick|\"
            assign: reply

  - result: \"{{reply}}\"
";

pub fn run(cmd: Cmd) {
    match cmd {
        Cmd::Yaml { template } => print!("{}", template.body()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_compiles(template: TemplateKind) {
        let body = template.body();
        relix_runtime::yaml_flow::compile_source(body)
            .unwrap_or_else(|e| panic!("scaffold {template:?} failed to compile: {e}\n{body}"));
    }

    #[test]
    fn chat_template_compiles_through_yaml_frontend() {
        assert_compiles(TemplateKind::Chat);
    }

    #[test]
    fn stream_template_compiles_through_yaml_frontend() {
        assert_compiles(TemplateKind::Stream);
    }

    #[test]
    fn try_template_compiles_through_yaml_frontend() {
        assert_compiles(TemplateKind::Try);
    }

    #[test]
    fn loop_template_compiles_through_yaml_frontend() {
        assert_compiles(TemplateKind::Loop);
    }

    #[test]
    fn chat_template_contains_the_required_keys() {
        let body = TemplateKind::Chat.body();
        for key in &[
            "steps:",
            "- call:",
            "peer:",
            "method:",
            "arg:",
            "assign:",
            "- result:",
        ] {
            assert!(
                body.contains(key),
                "chat scaffold should contain {key}: {body}"
            );
        }
    }

    #[test]
    fn stream_template_uses_stream_step() {
        let body = TemplateKind::Stream.body();
        assert!(
            body.contains("- stream:"),
            "stream scaffold missing `- stream:`: {body}"
        );
    }

    #[test]
    fn try_template_uses_multi_catch_with_three_kinds() {
        let body = TemplateKind::Try.body();
        assert!(
            body.contains("- try:"),
            "try scaffold missing `- try:`: {body}"
        );
        assert!(
            body.contains("- kind: timeout"),
            "missing timeout clause: {body}"
        );
        assert!(
            body.contains("- kind: policy_denied"),
            "missing policy_denied clause: {body}"
        );
        assert!(body.contains("- kind: any"), "missing any clause: {body}");
    }

    #[test]
    fn loop_template_uses_counted_loop_step() {
        let body = TemplateKind::Loop.body();
        assert!(
            body.contains("- loop:"),
            "loop scaffold missing `- loop:`: {body}"
        );
        assert!(
            body.contains("times:"),
            "loop scaffold missing `times:`: {body}"
        );
    }

    #[test]
    fn every_template_starts_with_a_comment_so_operators_see_the_intent() {
        for t in [
            TemplateKind::Chat,
            TemplateKind::Stream,
            TemplateKind::Try,
            TemplateKind::Loop,
        ] {
            let body = t.body();
            assert!(
                body.trim_start().starts_with('#'),
                "{t:?} should start with a comment:\n{body}"
            );
        }
    }
}
