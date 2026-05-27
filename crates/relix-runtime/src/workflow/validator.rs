//! Workflow validation. Catches structural bugs the parser
//! can't see — undefined references, cycles in sequential
//! flows, missing peers, and unresolved variable
//! interpolations — and surfaces each with the offending
//! field name in the error message.
//!
//! Validation is intentionally conservative: it rejects what
//! is definitely broken (cycle in success-only chain,
//! reference to non-existent agent) without trying to prove
//! exhaustive coverage (a non-success edge that loops back is
//! a recovery loop, not a cycle).

use std::collections::{BTreeSet, HashSet};

use super::ast::{Edge, EdgeCondition, Workflow};

/// Workflow validation error. Each variant carries enough
/// context for the caller to render an operator-actionable
/// message naming the offending field.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ValidationError {
    #[error("flow.start `{0}` is not declared in the agents map")]
    StartAgentMissing(String),

    #[error("edge[{index}].from `{name}` is not declared in the agents map")]
    EdgeFromMissing { index: usize, name: String },

    #[error("edge[{index}].to `{name}` is not declared in the agents map")]
    EdgeToMissing { index: usize, name: String },

    #[error("cycle detected in success-path edges: {}", path.join(" → "))]
    CycleDetected { path: Vec<String> },

    #[error(
        "agent `{agent}` field `{field}` references undefined variable `{var}`. \
         Defined variables: workflow.input, {available}"
    )]
    UndefinedVariable {
        agent: String,
        field: String,
        var: String,
        available: String,
    },

    #[error(
        "flow.result references undefined variable `{var}`. \
         Defined variables: workflow.input, {available}"
    )]
    ResultUndefinedVariable { var: String, available: String },

    #[error("agent `{agent}` references peer alias `{peer}` not in peers config")]
    UnknownPeer { agent: String, peer: String },

    #[error("agent `{name}` output binding `{output}` collides with another agent's output")]
    DuplicateOutput { name: String, output: String },
}

/// Validate a parsed [`Workflow`]. When `known_peers` is
/// `Some`, every `agent.peer` is checked against the set;
/// when `None` the peer check is skipped (operator runs
/// `workflow.validate` without a peers config available —
/// the validate endpoint passes `None`).
pub fn validate(
    workflow: &Workflow,
    known_peers: Option<&BTreeSet<String>>,
) -> Result<(), ValidationError> {
    // 1. Start agent must exist.
    if !workflow.agents.contains_key(&workflow.flow.start) {
        return Err(ValidationError::StartAgentMissing(
            workflow.flow.start.clone(),
        ));
    }

    // 2. Every edge endpoint must exist.
    for (i, edge) in workflow.flow.edges.iter().enumerate() {
        if !workflow.agents.contains_key(&edge.from) {
            return Err(ValidationError::EdgeFromMissing {
                index: i,
                name: edge.from.clone(),
            });
        }
        if !workflow.agents.contains_key(&edge.to) {
            return Err(ValidationError::EdgeToMissing {
                index: i,
                name: edge.to.clone(),
            });
        }
    }

    // 3. Every agent's peer must exist in the peers config
    //    when one was supplied.
    if let Some(peers) = known_peers {
        for (name, spec) in &workflow.agents {
            if !peers.contains(&spec.peer) {
                return Err(ValidationError::UnknownPeer {
                    agent: name.clone(),
                    peer: spec.peer.clone(),
                });
            }
        }
    }

    // 4. Output names must be unique across agents.
    let mut seen_outputs: HashSet<String> = HashSet::new();
    for (name, spec) in &workflow.agents {
        if !seen_outputs.insert(spec.output.clone()) {
            return Err(ValidationError::DuplicateOutput {
                name: name.clone(),
                output: spec.output.clone(),
            });
        }
    }

    // 5. Variable references in inputs + result must be
    //    defined. The reachability check determines which
    //    variables are visible to each step using the
    //    success-edge DAG; non-reachable references fail.
    let visibility = compute_visibility(workflow);
    let available_globally: Vec<String> = std::iter::once("workflow.input".to_string())
        .chain(
            workflow
                .agents
                .values()
                .map(|a| format!("{}.output", a.output)),
        )
        .collect();

    for (name, spec) in &workflow.agents {
        let reachable_outputs: BTreeSet<&String> = visibility
            .get(name)
            .map(|s| s.iter().collect())
            .unwrap_or_default();
        for var in extract_vars(&spec.input) {
            if !is_var_visible(&var, name, &reachable_outputs, workflow) {
                return Err(ValidationError::UndefinedVariable {
                    agent: name.clone(),
                    field: "input".to_string(),
                    var,
                    available: available_globally.join(", "),
                });
            }
        }
    }

    if let Some(result_tmpl) = workflow.flow.result.as_ref() {
        // Result can read any output — it runs at the END of
        // the workflow, after every reachable step has had a
        // chance to run.
        let all_outputs: BTreeSet<&String> = workflow.agents.values().map(|a| &a.output).collect();
        for var in extract_vars(result_tmpl) {
            if !is_var_visible_result(&var, &all_outputs) {
                return Err(ValidationError::ResultUndefinedVariable {
                    var,
                    available: available_globally.join(", "),
                });
            }
        }
    }

    // 6. Cycle detection on success-condition edges. Failure
    //    and Always edges are commonly used for recovery
    //    loops (retry agent on failure, always cleanup) so we
    //    don't treat those as cycles. Parallel edges fan-out
    //    rather than chain, and a cycle in pure-parallel land
    //    would mean an agent triggers its own parallel
    //    sibling — also flagged.
    detect_success_cycle(workflow)?;

    Ok(())
}

/// Variables visible to an agent step = outputs of every
/// agent that can REACH this step via success or always
/// edges from `flow.start`. Computed once via a fixed-point
/// pass and reused per-agent.
fn compute_visibility(workflow: &Workflow) -> std::collections::HashMap<String, BTreeSet<String>> {
    use std::collections::HashMap;
    use std::collections::VecDeque;

    let mut visibility: HashMap<String, BTreeSet<String>> = HashMap::new();
    // BFS from start, accumulating visible outputs along the
    // chain. A node is enqueued every time its visibility set
    // grows.
    let start = workflow.flow.start.clone();
    visibility.insert(start.clone(), BTreeSet::new());

    let mut q: VecDeque<String> = VecDeque::from([start]);
    while let Some(current) = q.pop_front() {
        let current_outputs = visibility.get(&current).cloned().unwrap_or_default();
        let current_agent_output = workflow.agents.get(&current).map(|a| a.output.clone());
        for edge in &workflow.flow.edges {
            if edge.from != current {
                continue;
            }
            if !matches!(
                edge.condition,
                EdgeCondition::Success | EdgeCondition::Always | EdgeCondition::Parallel
            ) {
                // Failure edges go to error handlers; the
                // error handler's input gets workflow.input
                // and the failed agent's output (the error
                // body), so failure handlers also see what
                // success chains see.
                continue;
            }
            let mut child_visibility = current_outputs.clone();
            if let Some(o) = current_agent_output.clone() {
                child_visibility.insert(o);
            }
            let entry = visibility.entry(edge.to.clone()).or_default();
            let prev_len = entry.len();
            for v in &child_visibility {
                entry.insert(v.clone());
            }
            // For failure edges from the same source: error
            // handlers also see the failed-step output.
            if entry.len() > prev_len {
                q.push_back(edge.to.clone());
            }
        }
        // Also propagate to failure-handler edges so they
        // see the upstream chain.
        for edge in &workflow.flow.edges {
            if edge.from != current || edge.condition != EdgeCondition::Failure {
                continue;
            }
            let mut child_visibility = current_outputs.clone();
            if let Some(o) = current_agent_output.clone() {
                child_visibility.insert(o);
            }
            let entry = visibility.entry(edge.to.clone()).or_default();
            let prev_len = entry.len();
            for v in &child_visibility {
                entry.insert(v.clone());
            }
            if entry.len() > prev_len {
                q.push_back(edge.to.clone());
            }
        }
    }
    visibility
}

fn is_var_visible(
    var: &str,
    own_name: &str,
    reachable_outputs: &BTreeSet<&String>,
    workflow: &Workflow,
) -> bool {
    if var == "workflow.input" {
        return true;
    }
    // `<step>.output` form.
    let stripped = var.strip_suffix(".output").unwrap_or(var);
    // A step cannot reference its OWN output (the output
    // doesn't exist until after the step runs).
    if let Some(own_spec) = workflow.agents.get(own_name)
        && own_spec.output == stripped
    {
        return false;
    }
    // The variable must match an output binding of a
    // reachable upstream step.
    reachable_outputs.iter().any(|o| **o == stripped)
}

fn is_var_visible_result(var: &str, all_outputs: &BTreeSet<&String>) -> bool {
    if var == "workflow.input" {
        return true;
    }
    let stripped = var.strip_suffix(".output").unwrap_or(var);
    all_outputs.iter().any(|o| **o == stripped)
}

/// Extract `{{name}}` markers from a template string.
/// Returns the trimmed identifiers. Whitespace inside the
/// braces is tolerated. Empty / non-identifier markers are
/// ignored (matching the SOL interpolator's behaviour) so
/// validation doesn't fire spuriously on literal-looking
/// braces.
pub fn extract_vars(template: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = template.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
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
                    if !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
                    {
                        out.push(name.to_string());
                    }
                    i = end + 2;
                    continue;
                }
                None => break,
            }
        }
        i += 1;
    }
    out
}

fn detect_success_cycle(workflow: &Workflow) -> Result<(), ValidationError> {
    // DFS from start following only Success / Always /
    // Parallel edges. Any back-edge in the recursion stack
    // is a cycle.
    let mut path: Vec<String> = Vec::new();
    let mut on_stack: HashSet<String> = HashSet::new();
    let mut visited: HashSet<String> = HashSet::new();
    dfs_cycle(
        &workflow.flow.start,
        workflow,
        &mut path,
        &mut on_stack,
        &mut visited,
    )
}

fn dfs_cycle(
    current: &str,
    workflow: &Workflow,
    path: &mut Vec<String>,
    on_stack: &mut HashSet<String>,
    visited: &mut HashSet<String>,
) -> Result<(), ValidationError> {
    if on_stack.contains(current) {
        let cycle_start = path.iter().position(|n| n == current).unwrap_or(0);
        let mut cycle: Vec<String> = path[cycle_start..].to_vec();
        cycle.push(current.to_string());
        return Err(ValidationError::CycleDetected { path: cycle });
    }
    if visited.contains(current) {
        return Ok(());
    }
    on_stack.insert(current.to_string());
    path.push(current.to_string());
    for edge in &workflow.flow.edges {
        if edge.from != current {
            continue;
        }
        if !matches!(
            edge.condition,
            EdgeCondition::Success | EdgeCondition::Always | EdgeCondition::Parallel
        ) {
            continue;
        }
        dfs_cycle(&edge.to, workflow, path, on_stack, visited)?;
    }
    on_stack.remove(current);
    path.pop();
    visited.insert(current.to_string());
    Ok(())
}

// Suppress an unused-import warning when the file is built
// in contexts that don't exercise `Edge`.
#[allow(dead_code)]
fn _ensure_edge_used(_: &Edge) {}
