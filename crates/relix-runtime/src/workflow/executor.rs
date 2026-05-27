//! Workflow executor. Drives a parsed + validated workflow
//! to completion, dispatching each step via a
//! [`WorkflowDispatcher`] and routing on success / failure /
//! parallel edges.
//!
//! Execution strategy: BFS-style work queue keyed on agent
//! step name. The queue starts with `flow.start`. Each
//! dequeued step interpolates its input from the running
//! variable bindings, dispatches the capability call, records
//! the outcome, and enqueues every outgoing edge whose
//! condition matches the result. Parallel edges from the same
//! source execute concurrently via `tokio::join_all`.
//!
//! The executor never panics on caller-recoverable errors.
//! Validation failures, dispatch failures, and missing
//! variable bindings all surface as a typed
//! [`WorkflowResult`] with `status == Failed` and a detail
//! string the caller can render.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use super::ast::{Edge, EdgeCondition, Workflow};
use super::dispatcher::{DispatchError, WorkflowDispatcher};

/// Opaque identifier for one workflow execution. Today this
/// is a hex-encoded random u128; the coordinator records the
/// id as the trace_id on its chronicle so `workflow.status`
/// can look up the trace later.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExecutionId(pub String);

impl ExecutionId {
    /// Mint a fresh execution id. Uses 16 random bytes
    /// rendered as hex (32 chars) so collisions across
    /// concurrent executions are astronomically unlikely.
    pub fn new() -> Self {
        let bytes: [u8; 16] = rand::random();
        Self(hex::encode(bytes))
    }
}

impl Default for ExecutionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ExecutionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Final status of an execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionStatus {
    /// Every required step ran successfully and the final
    /// result expression resolved.
    Success,
    /// At least one step failed and no failure-handling edge
    /// recovered it.
    Failed,
}

/// One step's contribution to the trace.
#[derive(Debug, Clone)]
pub struct ExecutionStep {
    /// Agent step name as declared in `agents:`.
    pub agent: String,
    /// Resolved peer alias.
    pub peer: String,
    /// Resolved capability method.
    pub capability: String,
    /// Interpolated input bytes (UTF-8 lossy for the trace).
    pub input: String,
    /// Response body (on success) or the error cause (on
    /// failure).
    pub output: String,
    /// Wall-clock duration of the dispatch call.
    pub latency_ms: u64,
    /// `Ok` on a successful dispatch; `Err(cause)` on a
    /// structured failure.
    pub outcome: Result<(), String>,
}

/// Complete execution trace. One step per dispatch, in
/// execution order. Parallel steps appear in the order they
/// finished — not the order they started — so trace
/// timestamps line up with chronicle ordering.
#[derive(Debug, Clone)]
pub struct ExecutionTrace {
    pub execution_id: ExecutionId,
    pub workflow_name: String,
    pub steps: Vec<ExecutionStep>,
    pub total_latency_ms: u64,
}

/// What the caller gets back from `WorkflowExecutor::run`.
#[derive(Debug, Clone)]
pub struct WorkflowResult {
    pub trace: ExecutionTrace,
    pub status: ExecutionStatus,
    /// Final result string. On success: the resolved
    /// `flow.result` template (or the last step's output
    /// when no result template is set). On failure: a
    /// descriptive error message naming the failed step and
    /// cause.
    pub result: String,
}

/// Top-level executor. Holds a reference to the workflow
/// AST + a dispatcher. One executor handles ONE workflow
/// run; create a fresh executor per execution.
pub struct WorkflowExecutor {
    workflow: Arc<Workflow>,
    dispatcher: Arc<dyn WorkflowDispatcher>,
}

impl WorkflowExecutor {
    /// Build a new executor. Takes an `Arc<Workflow>` so the
    /// workflow can be shared across concurrent parallel
    /// step executors without cloning the whole tree.
    pub fn new(workflow: Arc<Workflow>, dispatcher: Arc<dyn WorkflowDispatcher>) -> Self {
        Self {
            workflow,
            dispatcher,
        }
    }

    /// Execute the workflow against the given input string.
    /// Returns a typed [`WorkflowResult`] regardless of
    /// outcome — never panics, never propagates a dispatcher
    /// error past this boundary.
    pub async fn run(&self, input: &str) -> WorkflowResult {
        execute(self.workflow.clone(), self.dispatcher.clone(), input).await
    }
}

/// Free-function form of [`WorkflowExecutor::run`]. Useful
/// when the caller already has an `Arc<Workflow>` and an
/// `Arc<dyn WorkflowDispatcher>` and doesn't want the
/// executor struct as a holder.
pub async fn execute(
    workflow: Arc<Workflow>,
    dispatcher: Arc<dyn WorkflowDispatcher>,
    input: &str,
) -> WorkflowResult {
    let started = Instant::now();
    let execution_id = ExecutionId::new();
    let mut state = ExecutionState::new(input);
    let mut trace_steps: Vec<ExecutionStep> = Vec::new();

    // Run the start step + drive the BFS.
    let outcome = run_from(
        &workflow,
        dispatcher.clone(),
        &workflow.flow.start,
        &mut state,
        &mut trace_steps,
    )
    .await;

    let total_latency_ms = started.elapsed().as_millis() as u64;
    let trace = ExecutionTrace {
        execution_id: execution_id.clone(),
        workflow_name: workflow.name.clone(),
        steps: trace_steps,
        total_latency_ms,
    };

    match outcome {
        Ok(()) => {
            let final_result = render_final_result(&workflow, &state);
            WorkflowResult {
                trace,
                status: ExecutionStatus::Success,
                result: final_result,
            }
        }
        Err(message) => WorkflowResult {
            trace,
            status: ExecutionStatus::Failed,
            result: message,
        },
    }
}

/// Mutable execution state passed through the BFS.
struct ExecutionState {
    /// Map of `{{workflow.input}}` and `{{<output>.output}}`
    /// to their materialised string values.
    bindings: BTreeMap<String, String>,
    /// Names of agent steps already executed. Prevents
    /// infinite re-entry on diamond patterns where two
    /// success edges converge on a single step.
    visited: HashSet<String>,
}

impl ExecutionState {
    fn new(input: &str) -> Self {
        let mut bindings = BTreeMap::new();
        bindings.insert("workflow.input".to_string(), input.to_string());
        Self {
            bindings,
            visited: HashSet::new(),
        }
    }

    fn bind(&mut self, output_name: &str, value: String) {
        self.bindings.insert(format!("{output_name}.output"), value);
    }
}

/// Run one agent step + recurse into outgoing edges that
/// match the outcome. Async because dispatch and parallel
/// fan-out both need awaiting. Returns `Err(cause)` when the
/// current step (or any downstream step) failed without a
/// failure-handling edge.
fn run_from<'a>(
    workflow: &'a Workflow,
    dispatcher: Arc<dyn WorkflowDispatcher>,
    agent_name: &'a str,
    state: &'a mut ExecutionState,
    trace: &'a mut Vec<ExecutionStep>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        match run_step_only(workflow, dispatcher.clone(), agent_name, state, trace).await {
            StepRunResult::Skipped => Ok(()),
            StepRunResult::Ran(outcome) => {
                follow_edges(workflow, dispatcher, agent_name, outcome, state, trace).await
            }
            StepRunResult::Aborted(message) => Err(message),
        }
    })
}

/// Result of running ONE step in isolation (no edge
/// following). Used by [`run_from`] for sequential calls and
/// by the parallel-fan-out path to run a sibling without
/// recursing into its descendants (descendants run AFTER all
/// siblings merge, so the join sees every sibling's output).
enum StepRunResult {
    /// Step was already in the visited set; nothing to do.
    Skipped,
    /// Step ran to completion. Carries the resolved outcome
    /// so the caller can route on Success / Failure.
    Ran(StepOutcome),
    /// Step couldn't run — e.g. its input referenced an
    /// undefined variable, or the agent name doesn't exist.
    /// Surfaced upward as a workflow-level error.
    Aborted(String),
}

/// Execute one step: interpolate input, dispatch, bind
/// output, append trace. Does NOT follow outgoing edges.
async fn run_step_only(
    workflow: &Workflow,
    dispatcher: Arc<dyn WorkflowDispatcher>,
    agent_name: &str,
    state: &mut ExecutionState,
    trace: &mut Vec<ExecutionStep>,
) -> StepRunResult {
    if state.visited.contains(agent_name) {
        return StepRunResult::Skipped;
    }
    state.visited.insert(agent_name.to_string());

    let spec = match workflow.agents.get(agent_name) {
        Some(s) => s,
        None => {
            return StepRunResult::Aborted(format!(
                "workflow references undefined agent `{agent_name}`"
            ));
        }
    };
    let input_str = match interpolate(&spec.input, &state.bindings) {
        Ok(v) => v,
        Err(missing) => {
            return StepRunResult::Aborted(format!(
                "agent `{agent_name}` input references undefined variable `{missing}`"
            ));
        }
    };
    let started_at = Instant::now();
    let dispatch_outcome = dispatcher
        .dispatch(&spec.peer, &spec.capability, input_str.as_bytes())
        .await;
    let latency_ms = started_at.elapsed().as_millis() as u64;

    match dispatch_outcome {
        Ok(body) => {
            let body_str = String::from_utf8_lossy(&body).to_string();
            state.bind(&spec.output, body_str.clone());
            trace.push(ExecutionStep {
                agent: agent_name.to_string(),
                peer: spec.peer.clone(),
                capability: spec.capability.clone(),
                input: input_str,
                output: body_str,
                latency_ms,
                outcome: Ok(()),
            });
            StepRunResult::Ran(StepOutcome::Success)
        }
        Err(e) => {
            let cause = e.cause.clone();
            state.bind(&spec.output, cause.clone());
            trace.push(ExecutionStep {
                agent: agent_name.to_string(),
                peer: spec.peer.clone(),
                capability: spec.capability.clone(),
                input: input_str,
                output: cause.clone(),
                latency_ms,
                outcome: Err(cause),
            });
            StepRunResult::Ran(StepOutcome::Failed(e))
        }
    }
}

#[derive(Clone)]
enum StepOutcome {
    Success,
    Failed(DispatchError),
}

fn follow_edges<'a>(
    workflow: &'a Workflow,
    dispatcher: Arc<dyn WorkflowDispatcher>,
    from: &'a str,
    outcome: StepOutcome,
    state: &'a mut ExecutionState,
    trace: &'a mut Vec<ExecutionStep>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
    Box::pin(async move {
        // Partition the matching outgoing edges by mode.
        // Parallel edges fan out concurrently and converge
        // before descendants run; sequential edges chain
        // one-by-one and immediately recurse.
        let matching: Vec<&Edge> = workflow
            .flow
            .edges
            .iter()
            .filter(|e| e.from == from && matches_outcome(e.condition, &outcome))
            .collect();

        let (parallel, sequential): (Vec<&Edge>, Vec<&Edge>) = matching
            .into_iter()
            .partition(|e| e.condition == EdgeCondition::Parallel);

        // Sequential edges run one after another so each can
        // see the previous step's bindings.
        for edge in &sequential {
            run_from(workflow, dispatcher.clone(), &edge.to, state, trace).await?;
        }

        // Parallel edges: each sibling runs ONLY its single
        // step concurrently. After all siblings finish we
        // merge their bindings/trace/visited into the parent
        // state and THEN follow each sibling's outgoing
        // edges sequentially against the merged state. That
        // guarantees a join node downstream of multiple
        // parallel siblings sees every sibling's output
        // (the `visited` set keeps the join from running
        // twice).
        if !parallel.is_empty() {
            let snapshots: Vec<(BTreeMap<String, String>, HashSet<String>)> = parallel
                .iter()
                .map(|_| (state.bindings.clone(), state.visited.clone()))
                .collect();
            let mut futures = Vec::with_capacity(parallel.len());
            for (edge, (parent_bindings, parent_visited)) in parallel.iter().zip(snapshots) {
                let dispatcher = dispatcher.clone();
                let to = edge.to.clone();
                let wf: Arc<Workflow> = Arc::new(workflow.clone());
                futures.push(async move {
                    let mut local_state = ExecutionState {
                        bindings: parent_bindings,
                        visited: parent_visited,
                    };
                    let mut local_trace: Vec<ExecutionStep> = Vec::new();
                    let r = run_step_only(&wf, dispatcher, &to, &mut local_state, &mut local_trace)
                        .await;
                    (r, local_state, local_trace, to)
                });
            }
            let results = futures::future::join_all(futures).await;
            let mut first_error: Option<String> = None;
            let mut completed: Vec<(String, StepOutcome)> = Vec::new();
            for (res, local_state, mut local_trace, target) in results {
                trace.append(&mut local_trace);
                for (k, v) in local_state.bindings {
                    state.bindings.entry(k).or_insert(v);
                }
                for v in local_state.visited {
                    state.visited.insert(v);
                }
                match res {
                    StepRunResult::Ran(o) => completed.push((target, o)),
                    StepRunResult::Skipped => {}
                    StepRunResult::Aborted(e) => {
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                    }
                }
            }
            // Now follow each sibling's outgoing edges with
            // the merged state. The visited set prevents
            // re-execution of any join shared by siblings.
            for (target, sibling_outcome) in completed {
                follow_edges(
                    workflow,
                    dispatcher.clone(),
                    &target,
                    sibling_outcome,
                    state,
                    trace,
                )
                .await?;
            }
            if let Some(e) = first_error {
                return Err(e);
            }
        }

        // A bare failure with no matching failure / always
        // edge propagates upward as a workflow-level error.
        if sequential.is_empty()
            && parallel.is_empty()
            && let StepOutcome::Failed(err) = outcome
        {
            return Err(format!(
                "workflow step `{from}` failed with no failure handler: {err}"
            ));
        }
        Ok(())
    })
}

fn matches_outcome(cond: EdgeCondition, outcome: &StepOutcome) -> bool {
    matches!(
        (cond, outcome),
        (EdgeCondition::Success, StepOutcome::Success)
            | (EdgeCondition::Failure, StepOutcome::Failed(_))
            | (EdgeCondition::Always, _)
            | (EdgeCondition::Parallel, StepOutcome::Success)
    )
}

fn render_final_result(workflow: &Workflow, state: &ExecutionState) -> String {
    if let Some(template) = workflow.flow.result.as_ref() {
        match interpolate(template, &state.bindings) {
            Ok(s) => s,
            Err(missing) => format!("<unresolved variable `{missing}` in flow.result>"),
        }
    } else {
        // No result template — use the last bound output.
        state
            .bindings
            .iter()
            .rfind(|(k, _)| *k != "workflow.input")
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }
}

/// Interpolate `{{name}}` markers in `template` from the
/// bindings map. Returns `Err(name)` when a referenced
/// variable is missing. Unterminated / empty / non-identifier
/// markers are preserved verbatim (matches the SOL
/// interpolator's behaviour).
pub fn interpolate(template: &str, bindings: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let body_start = i + 2;
            let mut j = body_start;
            let mut closer = None;
            while j + 1 < bytes.len() {
                if bytes[j] == b'}' && bytes[j + 1] == b'}' {
                    closer = Some(j);
                    break;
                }
                j += 1;
            }
            match closer {
                Some(end) => {
                    let raw = std::str::from_utf8(&bytes[body_start..end]).unwrap_or("");
                    let name = raw.trim();
                    let is_ident = !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == '.');
                    if !is_ident {
                        // Preserve verbatim so operators see
                        // the typo in their template.
                        out.push_str(std::str::from_utf8(&bytes[i..end + 2]).unwrap_or(""));
                        i = end + 2;
                        continue;
                    }
                    let value = bindings.get(name).ok_or_else(|| name.to_string())?;
                    out.push_str(value);
                    i = end + 2;
                    continue;
                }
                None => {
                    // Unterminated `{{` — preserve verbatim.
                    out.push_str(std::str::from_utf8(&bytes[i..]).unwrap_or(""));
                    i = bytes.len();
                    continue;
                }
            }
        }
        let ch = template[i..].chars().next().unwrap();
        let n = ch.len_utf8();
        out.push(ch);
        i += n;
    }
    Ok(out)
}
