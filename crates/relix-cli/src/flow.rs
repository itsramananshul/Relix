//! `relix-cli flow ...` — workflow scaffolding helpers.
//!
//! Today only `flow yaml` is supported: it prints a minimal
//! YAML flow template to stdout. Developers can pipe it into
//! a file, edit peer/method/arg, and run it through
//! `relix-cli flow-run` without ever opening a tutorial.
//!
//! The template is intentionally tiny — a single
//! `remote_call` returning the response. It covers the
//! 80% case (call one peer, get a reply, return it) and
//! exposes the keys an operator will need to extend it
//! (more steps, `assign`, `stream`, `try` etc. all live in
//! `docs/yaml-flow-reference.md`).

use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Print a minimal working YAML flow template to stdout.
    /// Pipe into a file (e.g. `relix flow yaml > my.yml`),
    /// edit the peer/method/arg, then run it through
    /// `relix-cli flow-run --flow my.yml ...`.
    Yaml,
}

/// Minimal YAML flow template — single remote_call + result
/// pattern an operator can adapt in under five minutes.
/// Uses local `let` steps to seed the session id and the
/// user message so the scaffold compiles AND runs without
/// any external substitution. Operators editing this for a
/// bridge-rendered template just replace the seeded values
/// with `{{SESSION}}` / `{{MESSAGE}}` placeholders (the
/// bridge does the string substitution before the flow
/// hits the compiler — see `flows/chat_template.yml`).
const YAML_TEMPLATE: &str = "# Minimal Relix YAML flow. Fill in your peer + method + arg.
#
# Steps execute top-down. `call` runs a unary capability;
# `stream` is the streaming variant; `assign:` binds the
# response to a variable. See docs/yaml-flow-reference.md
# for the full surface (let, if, loop, try, catch, ...).

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

pub fn run(cmd: Cmd) {
    match cmd {
        Cmd::Yaml => print!("{YAML_TEMPLATE}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_template_compiles_through_yaml_frontend() {
        // The scaffold output MUST be a valid YAML flow.
        // Anything the operator gets here should run through
        // the compiler cleanly so they don't hit a parse
        // error on their very first edit-cycle.
        let bc = relix_runtime::yaml_flow::compile_source(YAML_TEMPLATE)
            .unwrap_or_else(|e| panic!("scaffold YAML failed to compile: {e}"));
        assert!(!bc.is_empty(), "scaffold must produce non-empty bytecode");
    }

    #[test]
    fn yaml_template_contains_the_required_keys() {
        // Smoke check: every operator-facing field that
        // distinguishes a real YAML flow must be in the
        // emitted text. Catches accidental template
        // truncation.
        assert!(YAML_TEMPLATE.contains("steps:"));
        assert!(YAML_TEMPLATE.contains("- call:"));
        assert!(YAML_TEMPLATE.contains("peer:"));
        assert!(YAML_TEMPLATE.contains("method:"));
        assert!(YAML_TEMPLATE.contains("arg:"));
        assert!(YAML_TEMPLATE.contains("assign:"));
        assert!(YAML_TEMPLATE.contains("- result:"));
    }

    #[test]
    fn yaml_template_starts_with_a_comment_so_operators_see_the_intent() {
        assert!(
            YAML_TEMPLATE.trim_start().starts_with('#'),
            "first non-blank line should be a comment"
        );
    }
}
