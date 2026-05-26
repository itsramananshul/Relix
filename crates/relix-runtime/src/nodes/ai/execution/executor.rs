//! Executor — thin state machine that walks an
//! [`ExecutionPlan`] step-by-step and captures evidence as it
//! goes. The executor itself does NOT call models or tools;
//! the caller dispatches each step and feeds the result
//! back through [`Executor::advance`]. This shape keeps the
//! state machine deterministic + trivially testable.
//!
//! Evidence capture lives in [`EvidenceRecord`] — the
//! coordinator chronicle receives one per ai.chat call when
//! `[execution] evidence_capture = true` is configured.

use serde::{Deserialize, Serialize};

use super::planner::ExecutionPlan;

/// Result of executing a single plan step. The executor
/// stores one of these per step in `ExecutionState`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepResult {
    Ok { output: String },
    Err { reason: String },
    Skipped { reason: String },
}

impl StepResult {
    /// Short operator-facing summary. Used by the evidence
    /// record + chronicle preview.
    pub fn summarise(&self) -> String {
        match self {
            Self::Ok { output } => {
                let preview: String = output.chars().take(80).collect();
                format!("ok: {preview}")
            }
            Self::Err { reason } => format!("err: {reason}"),
            Self::Skipped { reason } => format!("skipped: {reason}"),
        }
    }
}

/// Live state of an in-flight execution.
#[derive(Clone, Debug)]
pub struct ExecutionState {
    pub plan: ExecutionPlan,
    pub current_step: usize,
    pub step_results: Vec<StepResult>,
    pub started_at: i64,
}

impl ExecutionState {
    pub fn new(plan: ExecutionPlan) -> Self {
        Self {
            plan,
            current_step: 0,
            step_results: Vec::new(),
            started_at: unix_secs(),
        }
    }
}

/// Pure-function state-machine driver.
pub struct Executor;

impl Executor {
    /// Append the result of the current step and advance the
    /// pointer to the next one. Returns a reference to the
    /// stored result so callers can keep using it without
    /// re-borrowing the state.
    pub fn advance(state: &mut ExecutionState, step_output: StepResult) -> &StepResult {
        state.step_results.push(step_output);
        state.current_step += 1;
        state
            .step_results
            .last()
            .expect("just pushed a result, vec is non-empty")
    }

    /// `true` once every plan step has a recorded result.
    pub fn is_complete(state: &ExecutionState) -> bool {
        state.step_results.len() >= state.plan.steps.len()
    }

    /// Multi-line summary suitable for the chronicle. Lines
    /// are `<n>. <step description> -> <result summary>`.
    pub fn collect_evidence(state: &ExecutionState) -> String {
        let mut out = String::new();
        for (i, step) in state.plan.steps.iter().enumerate() {
            let result_summary = state
                .step_results
                .get(i)
                .map(|r| r.summarise())
                .unwrap_or_else(|| "pending".to_string());
            out.push_str(&format!(
                "{n}. {step} -> {result}\n",
                n = i + 1,
                step = step.describe(),
                result = result_summary
            ));
        }
        out
    }
}

/// Chronicle-shaped record built from a completed
/// [`ExecutionState`]. Serialised to JSON and appended to
/// the coordinator's chronicle as
/// `ai.execution_evidence`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EvidenceRecord {
    pub task_id: String,
    pub session_id: String,
    pub plan_steps: Vec<String>,
    pub step_results: Vec<String>,
    pub reversibility: String,
    pub approved_by: Option<String>,
    pub started_at: i64,
    pub finished_at: i64,
}

impl EvidenceRecord {
    /// Build from a completed (or partial) [`ExecutionState`].
    pub fn from_state(
        state: &ExecutionState,
        task_id: &str,
        session_id: &str,
        approved_by: Option<String>,
    ) -> Self {
        let plan_steps = state.plan.steps.iter().map(|s| s.describe()).collect();
        let step_results = state.step_results.iter().map(|r| r.summarise()).collect();
        Self {
            task_id: task_id.to_string(),
            session_id: session_id.to_string(),
            plan_steps,
            step_results,
            reversibility: state.plan.reversibility.as_str().to_string(),
            approved_by,
            started_at: state.started_at,
            finished_at: unix_secs(),
        }
    }

    /// Convenience: serialise to a JSON string. Used by the
    /// chronicle-append hook; tests assert the wire shape
    /// here.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::super::planner::{ExecutionPlan, PlanStep, Reversibility};
    use super::*;

    fn three_step_plan() -> ExecutionPlan {
        ExecutionPlan {
            steps: vec![
                PlanStep::ModelCall {
                    prompt: "hi".into(),
                    model: "m".into(),
                },
                PlanStep::ToolCall {
                    tool: "web.fetch".into(),
                    args: "https://x".into(),
                },
                PlanStep::MemoryWrite {
                    content: "note".into(),
                },
            ],
            estimated_cost_cents: 0,
            requires_approval: false,
            reversibility: Reversibility::Reversible,
        }
    }

    #[test]
    fn advance_appends_result_and_moves_pointer() {
        let mut state = ExecutionState::new(three_step_plan());
        let r = Executor::advance(
            &mut state,
            StepResult::Ok {
                output: "modeled".into(),
            },
        );
        assert!(matches!(r, StepResult::Ok { .. }));
        assert_eq!(state.current_step, 1);
        assert_eq!(state.step_results.len(), 1);
    }

    #[test]
    fn is_complete_flips_when_every_step_has_a_result() {
        let mut state = ExecutionState::new(three_step_plan());
        assert!(!Executor::is_complete(&state));
        for _ in 0..3 {
            Executor::advance(
                &mut state,
                StepResult::Ok {
                    output: "ok".into(),
                },
            );
        }
        assert!(Executor::is_complete(&state));
    }

    #[test]
    fn collect_evidence_renders_step_and_result_lines() {
        let mut state = ExecutionState::new(three_step_plan());
        Executor::advance(
            &mut state,
            StepResult::Ok {
                output: "model said hi".into(),
            },
        );
        Executor::advance(
            &mut state,
            StepResult::Err {
                reason: "tool timeout".into(),
            },
        );
        // Third step still pending.
        let evidence = Executor::collect_evidence(&state);
        assert!(evidence.contains("1. model_call(m) -> ok: model said hi"));
        assert!(evidence.contains("2. tool_call(web.fetch) -> err: tool timeout"));
        assert!(evidence.contains("3. memory_write -> pending"));
    }

    #[test]
    fn evidence_record_serialises_to_valid_json() {
        let mut state = ExecutionState::new(three_step_plan());
        Executor::advance(
            &mut state,
            StepResult::Ok {
                output: "hi".into(),
            },
        );
        let rec = EvidenceRecord::from_state(
            &state,
            "task-1",
            "sess-1",
            Some("operator@example.com".into()),
        );
        let json = rec.to_json();
        // Round-trip via serde so a future schema break here
        // would fail the test.
        let back: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(back["task_id"], "task-1");
        assert_eq!(back["session_id"], "sess-1");
        assert_eq!(back["approved_by"], "operator@example.com");
        assert_eq!(back["reversibility"], "reversible");
        assert!(back["plan_steps"].is_array());
        assert!(back["step_results"].is_array());
    }

    #[test]
    fn step_result_summarise_truncates_long_output() {
        let big = "x".repeat(500);
        let s = StepResult::Ok { output: big };
        let summary = s.summarise();
        assert!(summary.starts_with("ok: "));
        // The summary takes 80 chars from the output plus
        // the "ok: " prefix — well under the raw length.
        assert!(summary.len() < 500);
    }

    #[test]
    fn evidence_record_without_approver_serialises_null() {
        let state = ExecutionState::new(three_step_plan());
        let rec = EvidenceRecord::from_state(&state, "t", "s", None);
        let json = rec.to_json();
        assert!(json.contains("\"approved_by\":null"));
    }
}
