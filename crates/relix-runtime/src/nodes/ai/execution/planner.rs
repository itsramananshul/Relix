//! Planner — turns a raw model reply into a structured
//! [`ExecutionPlan`] and classifies the plan's reversibility.
//!
//! Honest scope: the parser is deliberately conservative. It
//! recognises one structured form (`<plan>…</plan>` blocks
//! with `tool: name\nargs: …\n` entries) plus a handful of
//! single-line directives. Anything else is wrapped in a
//! single-step plan whose only step is a `ModelCall` carrying
//! the verbatim response. That keeps existing chat traffic
//! flowing through the new layer without forcing every model
//! to learn a plan grammar.

use serde::{Deserialize, Serialize};

/// One concrete step in an execution plan. Each variant
/// carries everything the executor needs to dispatch the
/// step without re-parsing the response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanStep {
    ModelCall { prompt: String, model: String },
    ToolCall { tool: String, args: String },
    MemoryRead { query: String },
    MemoryWrite { content: String },
    HumanApproval { reason: String },
}

impl PlanStep {
    /// Short operator-facing description, used by the
    /// chronicle + the evidence record so humans don't have
    /// to read the full args / prompt.
    pub fn describe(&self) -> String {
        match self {
            Self::ModelCall { model, .. } => format!("model_call({model})"),
            Self::ToolCall { tool, .. } => format!("tool_call({tool})"),
            Self::MemoryRead { .. } => "memory_read".to_string(),
            Self::MemoryWrite { .. } => "memory_write".to_string(),
            Self::HumanApproval { reason } => {
                let preview: String = reason.chars().take(60).collect();
                format!("human_approval: {preview}")
            }
        }
    }

    /// `true` when this step type cannot be undone by the
    /// runtime without operator intervention. Used by
    /// [`Planner::classify_reversibility`].
    fn is_irreversible(&self) -> bool {
        match self {
            Self::ToolCall { tool, .. } => irreversible_tool(tool),
            // ModelCall is a read of a stateless service.
            // MemoryWrite is reversible because the layered
            // memory store tracks bi-temporal validity (a
            // future correction can `invalidate` the row).
            // HumanApproval / MemoryRead never mutate.
            _ => false,
        }
    }
}

/// Reversibility verdict for a plan as a whole.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reversibility {
    Reversible,
    PartiallyReversible { steps_reversible: Vec<usize> },
    Irreversible,
}

impl Reversibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reversible => "reversible",
            Self::PartiallyReversible { .. } => "partially_reversible",
            Self::Irreversible => "irreversible",
        }
    }
}

/// The full structured plan returned by the planner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub steps: Vec<PlanStep>,
    pub estimated_cost_cents: u32,
    pub requires_approval: bool,
    pub reversibility: Reversibility,
}

/// Pure-function planner.
pub struct Planner;

impl Planner {
    /// Parse a raw model reply into an `ExecutionPlan`.
    ///
    /// Recognised shapes:
    /// - `<plan>` block delimited by `<plan>` / `</plan>`
    ///   with `tool: <name>\nargs: <args>` entries.
    /// - Any other content folds into a single `ModelCall`
    ///   carrying the verbatim response — that's the path
    ///   the current `handle_chat` exercises until tool
    ///   dispatch lands.
    pub fn parse_response(response: &str) -> ExecutionPlan {
        let mut steps: Vec<PlanStep> = parse_plan_block(response).unwrap_or_default();
        if steps.is_empty() {
            steps.push(PlanStep::ModelCall {
                prompt: response.to_string(),
                model: String::new(),
            });
        }
        let reversibility = Self::classify_reversibility(&steps);
        let requires_approval = matches!(reversibility, Reversibility::Irreversible)
            || matches!(reversibility, Reversibility::PartiallyReversible { .. });
        ExecutionPlan {
            steps,
            estimated_cost_cents: 0,
            requires_approval,
            reversibility,
        }
    }

    /// Classify reversibility from a step list. `ToolCall`
    /// names that look mutating (write / delete / send /
    /// post / drop) flip the plan to irreversible; mixed
    /// plans report `PartiallyReversible` with the indices
    /// of the steps that *are* reversible so the chronicle
    /// can call them out.
    pub fn classify_reversibility(steps: &[PlanStep]) -> Reversibility {
        let mut irreversible_indices: Vec<usize> = Vec::new();
        let mut reversible_indices: Vec<usize> = Vec::new();
        for (i, s) in steps.iter().enumerate() {
            if s.is_irreversible() {
                irreversible_indices.push(i);
            } else {
                reversible_indices.push(i);
            }
        }
        match (
            irreversible_indices.is_empty(),
            reversible_indices.is_empty(),
        ) {
            (true, _) => Reversibility::Reversible,
            (false, true) => Reversibility::Irreversible,
            (false, false) => Reversibility::PartiallyReversible {
                steps_reversible: reversible_indices,
            },
        }
    }
}

/// Heuristic: tool name contains a mutating verb. Operators
/// can shape tool names to opt in (call something
/// `confirm_send_email`) or opt out (call it
/// `compose_email_draft`) — the heuristic is intentionally
/// keyword-driven so it's predictable.
fn irreversible_tool(tool: &str) -> bool {
    let lower = tool.to_ascii_lowercase();
    for kw in [
        "write",
        "delete",
        "remove",
        "send",
        "post",
        "drop",
        "destroy",
        "publish",
        "overwrite",
    ] {
        if lower.contains(kw) {
            return true;
        }
    }
    false
}

/// Parse a `<plan>…</plan>` block out of the response if
/// present. Returns `None` (not `Some(empty)`) when no block
/// is found so callers can distinguish "no plan" from "empty
/// plan."
fn parse_plan_block(response: &str) -> Option<Vec<PlanStep>> {
    let start = response.find("<plan>")?;
    let body_start = start + "<plan>".len();
    let end = response[body_start..].find("</plan>")?;
    let body = &response[body_start..body_start + end];
    let mut steps: Vec<PlanStep> = Vec::new();
    let mut current_tool: Option<String> = None;
    let mut current_args = String::new();
    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("tool:") {
            flush_pending_tool(&mut steps, &mut current_tool, &mut current_args);
            current_tool = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("args:") {
            current_args = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("memory_write:") {
            flush_pending_tool(&mut steps, &mut current_tool, &mut current_args);
            steps.push(PlanStep::MemoryWrite {
                content: rest.trim().to_string(),
            });
        } else if let Some(rest) = line.strip_prefix("memory_read:") {
            flush_pending_tool(&mut steps, &mut current_tool, &mut current_args);
            steps.push(PlanStep::MemoryRead {
                query: rest.trim().to_string(),
            });
        } else if let Some(rest) = line.strip_prefix("approval:") {
            flush_pending_tool(&mut steps, &mut current_tool, &mut current_args);
            steps.push(PlanStep::HumanApproval {
                reason: rest.trim().to_string(),
            });
        }
    }
    flush_pending_tool(&mut steps, &mut current_tool, &mut current_args);
    Some(steps)
}

fn flush_pending_tool(
    steps: &mut Vec<PlanStep>,
    current_tool: &mut Option<String>,
    current_args: &mut String,
) {
    if let Some(tool) = current_tool.take() {
        steps.push(PlanStep::ToolCall {
            tool,
            args: std::mem::take(current_args),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_response_yields_single_model_call_plan() {
        let plan = Planner::parse_response("hello there");
        assert_eq!(plan.steps.len(), 1);
        match &plan.steps[0] {
            PlanStep::ModelCall { prompt, .. } => assert_eq!(prompt, "hello there"),
            other => panic!("expected ModelCall, got {other:?}"),
        }
        assert!(matches!(plan.reversibility, Reversibility::Reversible));
        assert!(!plan.requires_approval);
    }

    #[test]
    fn plan_block_with_one_tool_parses_correctly() {
        let resp = "before\n<plan>\ntool: web.fetch\nargs: https://example.com\n</plan>\nafter";
        let plan = Planner::parse_response(resp);
        assert_eq!(plan.steps.len(), 1);
        match &plan.steps[0] {
            PlanStep::ToolCall { tool, args } => {
                assert_eq!(tool, "web.fetch");
                assert_eq!(args, "https://example.com");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn plan_block_with_multiple_steps_preserves_order() {
        let resp = "<plan>\n\
            tool: web.fetch\n\
            args: https://a\n\
            tool: web.fetch\n\
            args: https://b\n\
            memory_write: result a\n\
            </plan>";
        let plan = Planner::parse_response(resp);
        assert_eq!(plan.steps.len(), 3);
        assert!(matches!(plan.steps[0], PlanStep::ToolCall { .. }));
        assert!(matches!(plan.steps[2], PlanStep::MemoryWrite { .. }));
    }

    #[test]
    fn classify_reversibility_detects_irreversible_tool_call() {
        let steps = vec![PlanStep::ToolCall {
            tool: "email.send".into(),
            args: "to=a".into(),
        }];
        match Planner::classify_reversibility(&steps) {
            Reversibility::Irreversible => {}
            other => panic!("expected Irreversible, got {other:?}"),
        }
    }

    #[test]
    fn classify_reversibility_recognises_mixed_plan() {
        let steps = vec![
            PlanStep::ModelCall {
                prompt: "hi".into(),
                model: "gpt".into(),
            },
            PlanStep::ToolCall {
                tool: "fs.delete_file".into(),
                args: "/tmp/x".into(),
            },
            PlanStep::MemoryRead { query: "x".into() },
        ];
        match Planner::classify_reversibility(&steps) {
            Reversibility::PartiallyReversible { steps_reversible } => {
                // Indices 0 and 2 are reversible; index 1
                // (fs.delete_file) is not.
                assert_eq!(steps_reversible, vec![0, 2]);
            }
            other => panic!("expected PartiallyReversible, got {other:?}"),
        }
    }

    #[test]
    fn reversible_only_plan_returns_reversible() {
        let steps = vec![
            PlanStep::ModelCall {
                prompt: "hi".into(),
                model: "m".into(),
            },
            PlanStep::MemoryRead { query: "x".into() },
            PlanStep::MemoryWrite {
                content: "x".into(),
            },
            PlanStep::ToolCall {
                tool: "web.fetch".into(),
                args: "x".into(),
            },
        ];
        assert!(matches!(
            Planner::classify_reversibility(&steps),
            Reversibility::Reversible
        ));
    }

    #[test]
    fn parse_response_with_irreversible_plan_requires_approval() {
        let resp = "<plan>\ntool: email.send_all\nargs: list=ops\n</plan>";
        let plan = Planner::parse_response(resp);
        assert!(plan.requires_approval);
        assert!(matches!(plan.reversibility, Reversibility::Irreversible));
    }

    #[test]
    fn irreversible_tool_keyword_matches_are_case_insensitive() {
        assert!(irreversible_tool("DB.DROP_TABLE"));
        assert!(irreversible_tool("MailSendAll"));
        assert!(irreversible_tool("fs.delete"));
        assert!(!irreversible_tool("web.fetch"));
        assert!(!irreversible_tool("memory.search"));
    }

    #[test]
    fn plan_step_describe_returns_operator_readable_text() {
        let s = PlanStep::ToolCall {
            tool: "email.send".into(),
            args: "to=a".into(),
        };
        assert_eq!(s.describe(), "tool_call(email.send)");
        let s = PlanStep::HumanApproval {
            reason: "Confirm send".into(),
        };
        assert!(s.describe().starts_with("human_approval:"));
    }
}
