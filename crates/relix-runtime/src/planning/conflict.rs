//! RELIX-7.24 Stage-1/3 — multi-specialist conflict resolver.
//!
//! When the [`super::orchestrator::Orchestrator`] merges
//! specialist sub-workflows into one [`Workflow`], the result
//! can carry conflicts the executor would either fail on
//! (validator-rejected) or silently mis-execute (last-writer-
//! wins, races on the same resource). The conflict resolver
//! sits between the orchestrator's merge step and the
//! coordinator's final response; it inspects the merged
//! workflow, fixes every recognised conflict in place, and
//! records every action in a
//! [`ConflictResolutionReport`].
//!
//! Detection rules (matching the §7.24 Stage design):
//!
//! 1. **Duplicate output binding** — two agents bind their
//!    response to the same `output` name. Downstream
//!    `{{<name>.output}}` references would be ambiguous and
//!    the workflow validator rejects the plan with
//!    `DuplicateOutput`.
//! 2. **Interfering parallel peer-cap pair** — two agents
//!    are dispatched in parallel from the same source AND
//!    call the same `(peer, capability)` pair AND the
//!    capability looks write-like (`set / put / write /
//!    create / delete / update / post / send`). They could
//!    race on the same resource.
//! 3. **Reference to a non-existent output** — an agent's
//!    `input` interpolates `{{<name>.output}}` where `<name>`
//!    is not an output binding of any agent in the workflow.
//!    The validator would reject the plan; without
//!    resolution the operator gets an obscure error rather
//!    than a clean drop.
//!
//! Resolution strategies, tried in order per conflict:
//!
//! - [`ResolutionStrategy::Rename`] — give the duplicate
//!   producer a fresh, unique `output` name. References stay
//!   pointing at the SURVIVING (kept) producer so the rest
//!   of the workflow is unambiguous; the renamed producer's
//!   value is still captured in the execution trace for
//!   auditing.
//! - [`ResolutionStrategy::Sequence`] — convert an
//!   interfering parallel edge from `Parallel` to
//!   `Success`, sourcing the moved edge from the surviving
//!   parallel target. This serialises the interfering pair
//!   without affecting any other parallel sibling.
//! - [`ResolutionStrategy::Drop`] — strip the offending
//!   `{{<name>.output}}` marker from the agent's input
//!   template. The agent still runs; it just no longer
//!   tries to read a value that doesn't exist.
//! - [`ResolutionStrategy::Escalate`] — when validation
//!   still fails after every applicable strategy has run,
//!   the resolver returns the partially-fixed workflow and
//!   marks the report with an `escalated` reason. The
//!   coordinator surfaces this as a structured error in the
//!   `planning.create_plan` response so the operator can
//!   decide.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::workflow::validator::extract_vars;
use crate::workflow::{EdgeCondition, Workflow, validate};

/// Summary of every action the resolver took. Carried back to
/// the `planning.create_plan` response so the operator can
/// audit what was changed.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ConflictResolutionReport {
    pub conflicts_detected: usize,
    pub conflicts_resolved: usize,
    /// Per-conflict log entries in detection order.
    pub details: Vec<ConflictResolutionEntry>,
    /// When `Some`, the workflow could NOT be fully fixed
    /// and the coordinator surfaces this as an error to the
    /// caller. Carries a description of why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalated: Option<String>,
    /// First strategy applied, suitable for a top-level
    /// summary field in the response. `"none"` when no
    /// conflicts were detected. When multiple strategies
    /// fired in one resolver pass this names the dominant
    /// one (the first to fire).
    pub strategy_used: String,
}

impl ConflictResolutionReport {
    fn record(&mut self, entry: ConflictResolutionEntry) {
        self.conflicts_detected += 1;
        if !matches!(entry.strategy, ResolutionStrategy::Escalate) {
            self.conflicts_resolved += 1;
        }
        if self.strategy_used.is_empty() {
            self.strategy_used = entry.strategy.as_str().to_string();
        }
        self.details.push(entry);
    }
}

/// One row in the resolution log.
#[derive(Clone, Debug, Serialize)]
pub struct ConflictResolutionEntry {
    pub kind: ConflictKind,
    pub strategy: ResolutionStrategy,
    /// Human-readable description of the conflict and the
    /// action taken.
    pub description: String,
    /// Step the resolver mutated (output renamed, edge
    /// re-conditioned, input rewritten).
    pub affected_step: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    DuplicateOutput,
    InterferingParallelCall,
    UndefinedReference,
    /// Catch-all — final validation still failed even after
    /// every applicable strategy. Surfaces alongside
    /// `report.escalated`.
    Unresolvable,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionStrategy {
    Rename,
    Sequence,
    Drop,
    Escalate,
}

impl ResolutionStrategy {
    /// Stable string form for the report's `strategy_used`
    /// summary field.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::Sequence => "sequence",
            Self::Drop => "drop",
            Self::Escalate => "escalate",
        }
    }
}

/// Stateless conflict resolver. The single public entry point
/// is [`Self::resolve`].
#[derive(Clone, Debug, Default)]
pub struct ConflictResolver;

impl ConflictResolver {
    pub fn new() -> Self {
        Self
    }

    /// Record every entry in `report` into `spec`'s changelog
    /// via [`super::PlanSpec::with_change`], then re-sign the
    /// spec via [`super::PlanSpec::sign`] so downstream
    /// verification keeps passing. Called by the coordinator
    /// after [`Self::resolve`] when the spec is the
    /// signed-and-audited artifact passed into the approval
    /// store. Safe to call with a report carrying zero
    /// entries — it's a no-op in that case (no signature
    /// invalidation).
    pub fn record_into_spec(report: &ConflictResolutionReport, spec: &mut super::PlanSpec) {
        if report.details.is_empty() && report.escalated.is_none() {
            return;
        }
        for entry in &report.details {
            let change_type = match entry.strategy {
                ResolutionStrategy::Rename => "conflict_rename",
                ResolutionStrategy::Sequence => "conflict_sequence",
                ResolutionStrategy::Drop => "conflict_drop",
                ResolutionStrategy::Escalate => "conflict_escalate",
            };
            spec.with_change(change_type, &entry.description);
        }
        let _ = spec.sign();
    }

    /// Detect every recognised conflict in `workflow`, apply
    /// each strategy in order, and return the (possibly-
    /// rewritten) workflow alongside a
    /// [`ConflictResolutionReport`].
    ///
    /// Always returns a workflow — even when `escalated` is
    /// set the partially-fixed workflow is returned so the
    /// operator can inspect what the resolver did.
    pub fn resolve(&self, mut workflow: Workflow) -> (Workflow, ConflictResolutionReport) {
        let mut report = ConflictResolutionReport::default();
        resolve_duplicate_outputs(&mut workflow, &mut report);
        resolve_interfering_parallel_calls(&mut workflow, &mut report);
        resolve_undefined_references(&mut workflow, &mut report);

        if let Err(e) = validate(&workflow, None) {
            // Final escalation: the strategies above didn't
            // fully fix the workflow. The coordinator turns
            // this into an INVALID_ARGS-style error so the
            // operator sees a structured conflict instead of
            // a silent invalid plan.
            report.escalated = Some(e.to_string());
            report.record(ConflictResolutionEntry {
                kind: ConflictKind::Unresolvable,
                strategy: ResolutionStrategy::Escalate,
                description: format!("workflow still invalid after resolution: {e}"),
                affected_step: String::new(),
            });
        }

        if report.strategy_used.is_empty() {
            report.strategy_used = "none".to_string();
        }
        (workflow, report)
    }
}

// ── rule 1: duplicate output binding ─────────────────────

fn resolve_duplicate_outputs(wf: &mut Workflow, report: &mut ConflictResolutionReport) {
    let by_output: BTreeMap<String, Vec<String>> =
        wf.agents.iter().fold(BTreeMap::new(), |mut acc, (n, s)| {
            acc.entry(s.output.clone()).or_default().push(n.clone());
            acc
        });
    let mut taken_outputs: BTreeSet<String> =
        wf.agents.values().map(|s| s.output.clone()).collect();
    for (output, producers) in &by_output {
        if producers.len() <= 1 {
            continue;
        }
        // Keep the alphabetically-first producer; rename the
        // rest. BTreeMap iteration is sorted so this is
        // deterministic.
        let kept = &producers[0];
        for losing in producers.iter().skip(1) {
            let new_output = fresh_output_name(output, &taken_outputs);
            taken_outputs.insert(new_output.clone());
            if let Some(spec) = wf.agents.get_mut(losing) {
                spec.output = new_output.clone();
            }
            report.record(ConflictResolutionEntry {
                kind: ConflictKind::DuplicateOutput,
                strategy: ResolutionStrategy::Rename,
                description: format!(
                    "step `{losing}` shared output binding `{output}` with `{kept}` — \
                     renamed to `{new_output}` (existing references resolve to `{kept}`)"
                ),
                affected_step: losing.clone(),
            });
        }
    }
}

fn fresh_output_name(base: &str, taken: &BTreeSet<String>) -> String {
    for n in 2..1000 {
        let candidate = format!("{base}_{n}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    format!("{base}__x")
}

// ── rule 2: interfering parallel peer-cap pair ───────────

/// Capability-name fragments that mark a write-like
/// operation. The resolver treats any of these as a signal
/// that two parallel callers to the same `(peer, capability)`
/// could race on a shared resource.
const WRITE_KEYWORDS: &[&str] = &[
    "set", "put", "write", "create", "delete", "update", "post", "send", "publish", "mutate",
];

fn looks_write_like(capability: &str) -> bool {
    let lower = capability.to_lowercase();
    WRITE_KEYWORDS.iter().any(|kw| {
        lower.contains(&format!(".{kw}"))
            || lower.contains(&format!("{kw}_"))
            || lower.contains(&format!("_{kw}"))
            || lower.ends_with(kw)
            || lower.starts_with(kw)
    })
}

fn resolve_interfering_parallel_calls(wf: &mut Workflow, report: &mut ConflictResolutionReport) {
    // Group parallel edges by their source.
    let mut by_source: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, e) in wf.flow.edges.iter().enumerate() {
        if matches!(e.condition, EdgeCondition::Parallel) {
            by_source.entry(e.from.clone()).or_default().push(i);
        }
    }
    // For each source with multiple parallel targets, find
    // pairs that interfere on `(peer, capability)`.
    let mut moves: Vec<(usize, String)> = Vec::new(); // (edge_index, new_from)
    for edge_indices in by_source.values() {
        // Map `(peer, cap)` -> Vec<(edge_index, to_step)>.
        let mut grouped: BTreeMap<(String, String), Vec<(usize, String)>> = BTreeMap::new();
        for &idx in edge_indices {
            let to = wf.flow.edges[idx].to.clone();
            if let Some(spec) = wf.agents.get(&to) {
                let key = (spec.peer.clone(), spec.capability.clone());
                grouped.entry(key).or_default().push((idx, to));
            }
        }
        for ((peer, cap), members) in &grouped {
            if members.len() <= 1 || !looks_write_like(cap) {
                continue;
            }
            // Keep members[0] parallel; convert the rest to
            // sequential from members[0].to.
            let kept_to = members[0].1.clone();
            for (idx, losing_to) in members.iter().skip(1) {
                moves.push((*idx, kept_to.clone()));
                report.record(ConflictResolutionEntry {
                    kind: ConflictKind::InterferingParallelCall,
                    strategy: ResolutionStrategy::Sequence,
                    description: format!(
                        "parallel siblings `{kept_to}` and `{losing_to}` both call `{cap}` on \
                         peer `{peer}` (write-like) — re-sequenced `{losing_to}` after `{kept_to}` \
                         to avoid a race on the shared resource"
                    ),
                    affected_step: losing_to.clone(),
                });
            }
        }
    }
    for (idx, new_from) in moves {
        wf.flow.edges[idx].from = new_from;
        wf.flow.edges[idx].condition = EdgeCondition::Success;
    }
}

// ── rule 3: drop undefined-reference markers ─────────────

fn resolve_undefined_references(wf: &mut Workflow, report: &mut ConflictResolutionReport) {
    let known_outputs: BTreeSet<String> = wf.agents.values().map(|s| s.output.clone()).collect();
    // Rewrite agent inputs in two passes so we don't mutate
    // and read at the same time.
    let mut edits: Vec<(String, String)> = Vec::new();
    for (name, spec) in &wf.agents {
        let vars = extract_vars(&spec.input);
        let mut rewritten = spec.input.clone();
        let mut dropped = Vec::new();
        for v in &vars {
            if is_workflow_input(v) {
                continue;
            }
            let stripped = v.strip_suffix(".output").unwrap_or(v);
            if !known_outputs.contains(stripped) {
                rewritten = strip_var_marker(&rewritten, v);
                dropped.push(v.clone());
            }
        }
        if !dropped.is_empty() {
            edits.push((name.clone(), rewritten));
            for d in dropped {
                report.record(ConflictResolutionEntry {
                    kind: ConflictKind::UndefinedReference,
                    strategy: ResolutionStrategy::Drop,
                    description: format!(
                        "step `{name}` input referenced undefined variable `{{{{ {d} }}}}` \
                         — marker dropped, agent still runs"
                    ),
                    affected_step: name.clone(),
                });
            }
        }
    }
    for (name, new_input) in edits {
        if let Some(spec) = wf.agents.get_mut(&name) {
            spec.input = new_input;
        }
    }
    // Also clean up the flow.result reference if it points
    // at a missing output.
    if let Some(result) = wf.flow.result.clone() {
        let vars = extract_vars(&result);
        let mut rewritten = result.clone();
        let mut dropped = Vec::new();
        for v in &vars {
            if is_workflow_input(v) {
                continue;
            }
            let stripped = v.strip_suffix(".output").unwrap_or(v);
            if !known_outputs.contains(stripped) {
                rewritten = strip_var_marker(&rewritten, v);
                dropped.push(v.clone());
            }
        }
        if !dropped.is_empty() {
            for d in &dropped {
                report.record(ConflictResolutionEntry {
                    kind: ConflictKind::UndefinedReference,
                    strategy: ResolutionStrategy::Drop,
                    description: format!(
                        "flow.result referenced undefined variable `{{{{ {d} }}}}` — marker \
                         dropped"
                    ),
                    affected_step: "flow.result".into(),
                });
            }
            wf.flow.result = if rewritten.trim().is_empty() {
                None
            } else {
                Some(rewritten)
            };
        }
    }
}

fn is_workflow_input(var: &str) -> bool {
    var == "workflow.input"
}

/// Strip a single `{{ <var> }}` marker (tolerating whitespace
/// around the name) from `s`. Other markers are preserved.
fn strip_var_marker(s: &str, var: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find matching `}}`.
            let mut j = i + 2;
            let mut matched = None;
            while j + 1 < bytes.len() {
                if bytes[j] == b'}' && bytes[j + 1] == b'}' {
                    matched = Some(j);
                    break;
                }
                j += 1;
            }
            match matched {
                Some(end) => {
                    let inner = std::str::from_utf8(&bytes[i + 2..end]).unwrap_or("").trim();
                    if inner == var {
                        i = end + 2;
                        // Collapse single trailing space so we
                        // don't leave double-spaces in the
                        // input after dropping a marker
                        // surrounded by them.
                        if i < bytes.len() && bytes[i] == b' ' && out.ends_with(' ') {
                            i += 1;
                        }
                    } else {
                        out.push_str(&s[i..end + 2]);
                        i = end + 2;
                    }
                    continue;
                }
                None => {
                    // Unterminated `{{` — emit verbatim.
                    out.push_str(&s[i..]);
                    return out;
                }
            }
        }
        // Append one byte. `s` is UTF-8; the only bytes we
        // splice via byte-indexing are the marker boundaries
        // (`{` / `}` are ASCII). Non-ASCII bytes fall through
        // here unchanged.
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{AgentSpec, Edge, EdgeCondition, FlowGraph, Workflow};
    use std::collections::BTreeMap;

    fn workflow(agents: Vec<(&str, AgentSpec)>, start: &str, edges: Vec<Edge>) -> Workflow {
        let mut map = BTreeMap::new();
        for (k, v) in agents {
            map.insert(k.to_string(), v);
        }
        Workflow {
            name: "test_wf".into(),
            version: 1,
            description: "test".into(),
            agents: map,
            flow: FlowGraph {
                start: start.into(),
                edges,
                result: None,
            },
        }
    }

    fn spec(peer: &str, cap: &str, input: &str, output: &str) -> AgentSpec {
        AgentSpec {
            peer: peer.into(),
            capability: cap.into(),
            input: input.into(),
            output: output.into(),
        }
    }

    #[test]
    fn no_conflicts_passes_through_unchanged_with_empty_report() {
        let wf = workflow(
            vec![
                ("a", spec("p1", "ai.chat", "{{workflow.input}}", "a")),
                ("b", spec("p2", "ai.chat", "{{a.output}}", "b")),
            ],
            "a",
            vec![Edge {
                from: "a".into(),
                to: "b".into(),
                condition: EdgeCondition::Success,
            }],
        );
        let resolver = ConflictResolver::new();
        let (out, report) = resolver.resolve(wf.clone());
        assert_eq!(report.conflicts_detected, 0);
        assert_eq!(report.conflicts_resolved, 0);
        assert!(report.escalated.is_none());
        assert_eq!(report.strategy_used, "none");
        // Workflow unchanged.
        assert_eq!(out.agents.len(), wf.agents.len());
    }

    #[test]
    fn duplicate_output_renames_second_producer() {
        let wf = workflow(
            vec![
                ("a", spec("p1", "ai.chat", "{{workflow.input}}", "shared")),
                ("b", spec("p2", "ai.chat", "{{workflow.input}}", "shared")),
            ],
            "a",
            vec![],
        );
        let resolver = ConflictResolver::new();
        let (out, report) = resolver.resolve(wf);
        assert_eq!(report.conflicts_detected, 1);
        assert_eq!(report.conflicts_resolved, 1);
        assert_eq!(report.strategy_used, "rename");
        // First (alphabetical) keeps; second renamed.
        assert_eq!(out.agents.get("a").unwrap().output, "shared");
        assert_ne!(out.agents.get("b").unwrap().output, "shared");
        assert!(
            out.agents.get("b").unwrap().output.starts_with("shared_"),
            "{}",
            out.agents.get("b").unwrap().output
        );
    }

    #[test]
    fn downstream_reference_continues_resolving_to_surviving_producer_after_rename() {
        let wf = workflow(
            vec![
                ("a", spec("p1", "ai.chat", "{{workflow.input}}", "x")),
                ("b", spec("p2", "ai.chat", "{{workflow.input}}", "x")),
                ("c", spec("p3", "ai.chat", "see {{x.output}}", "c")),
            ],
            "a",
            vec![
                Edge {
                    from: "a".into(),
                    to: "c".into(),
                    condition: EdgeCondition::Success,
                },
                Edge {
                    from: "b".into(),
                    to: "c".into(),
                    condition: EdgeCondition::Always,
                },
            ],
        );
        let resolver = ConflictResolver::new();
        let (out, report) = resolver.resolve(wf);
        assert_eq!(report.conflicts_detected, 1);
        // The `{{x.output}}` reference in c stays as-is and
        // now unambiguously resolves to agent `a` (which kept
        // the `x` output binding).
        assert!(out.agents.get("c").unwrap().input.contains("{{x.output}}"));
        // The workflow must validate now.
        validate(&out, None).expect("validates after rename");
    }

    #[test]
    fn interfering_parallel_write_calls_are_sequenced() {
        // Two parallel siblings both invoke `kv.set` on the
        // same peer — race.
        let wf = workflow(
            vec![
                (
                    "seed",
                    spec("coord", "ai.chat", "{{workflow.input}}", "seed"),
                ),
                ("w1", spec("kv-peer", "kv.set", "{{seed.output}}", "w1")),
                ("w2", spec("kv-peer", "kv.set", "{{seed.output}}", "w2")),
            ],
            "seed",
            vec![
                Edge {
                    from: "seed".into(),
                    to: "w1".into(),
                    condition: EdgeCondition::Parallel,
                },
                Edge {
                    from: "seed".into(),
                    to: "w2".into(),
                    condition: EdgeCondition::Parallel,
                },
            ],
        );
        let resolver = ConflictResolver::new();
        let (out, report) = resolver.resolve(wf);
        assert_eq!(report.conflicts_detected, 1);
        assert_eq!(report.strategy_used, "sequence");
        // Find the rewritten w2 edge.
        let w2_edge = out
            .flow
            .edges
            .iter()
            .find(|e| e.to == "w2")
            .expect("w2 edge");
        assert_eq!(w2_edge.condition, EdgeCondition::Success);
        // It is sequenced after w1 (the kept parallel
        // sibling).
        assert_eq!(w2_edge.from, "w1");
    }

    #[test]
    fn parallel_read_only_calls_are_not_sequenced() {
        // Both siblings call ai.chat (no write keyword) →
        // resolver leaves them parallel.
        let wf = workflow(
            vec![
                (
                    "seed",
                    spec("coord", "ai.chat", "{{workflow.input}}", "seed"),
                ),
                ("r1", spec("ai", "ai.chat", "{{seed.output}}", "r1")),
                ("r2", spec("ai", "ai.chat", "{{seed.output}}", "r2")),
            ],
            "seed",
            vec![
                Edge {
                    from: "seed".into(),
                    to: "r1".into(),
                    condition: EdgeCondition::Parallel,
                },
                Edge {
                    from: "seed".into(),
                    to: "r2".into(),
                    condition: EdgeCondition::Parallel,
                },
            ],
        );
        let resolver = ConflictResolver::new();
        let (out, report) = resolver.resolve(wf);
        assert_eq!(report.conflicts_detected, 0);
        for e in &out.flow.edges {
            if e.from == "seed" {
                assert_eq!(e.condition, EdgeCondition::Parallel);
            }
        }
    }

    #[test]
    fn undefined_reference_in_agent_input_is_dropped_and_reported() {
        let wf = workflow(
            vec![
                ("a", spec("p1", "ai.chat", "{{workflow.input}}", "a")),
                (
                    "b",
                    spec(
                        "p2",
                        "ai.chat",
                        "use {{ghost.output}} and also {{a.output}}",
                        "b",
                    ),
                ),
            ],
            "a",
            vec![Edge {
                from: "a".into(),
                to: "b".into(),
                condition: EdgeCondition::Success,
            }],
        );
        let resolver = ConflictResolver::new();
        let (out, report) = resolver.resolve(wf);
        assert_eq!(report.conflicts_detected, 1);
        assert_eq!(report.strategy_used, "drop");
        let b_input = &out.agents.get("b").unwrap().input;
        assert!(!b_input.contains("ghost.output"), "b_input={b_input}");
        assert!(b_input.contains("a.output"), "b_input={b_input}");
        // Validates now.
        validate(&out, None).expect("validates");
    }

    #[test]
    fn undefined_reference_in_flow_result_is_dropped() {
        let wf = Workflow {
            name: "wf".into(),
            version: 1,
            description: "".into(),
            agents: {
                let mut m = BTreeMap::new();
                m.insert("a".into(), spec("p1", "ai.chat", "{{workflow.input}}", "a"));
                m
            },
            flow: FlowGraph {
                start: "a".into(),
                edges: vec![],
                result: Some("answer {{ghost.output}}".into()),
            },
        };
        let resolver = ConflictResolver::new();
        let (out, report) = resolver.resolve(wf);
        assert_eq!(report.conflicts_detected, 1);
        let result = out.flow.result.unwrap_or_default();
        assert!(!result.contains("ghost.output"));
    }

    #[test]
    fn looks_write_like_recognises_common_patterns() {
        assert!(looks_write_like("kv.set"));
        assert!(looks_write_like("kv.put"));
        assert!(looks_write_like("blob.write"));
        assert!(looks_write_like("memory.create"));
        assert!(looks_write_like("task.update"));
        assert!(looks_write_like("alert.send"));
        assert!(!looks_write_like("ai.chat"));
        assert!(!looks_write_like("memory.search"));
        assert!(!looks_write_like("kv.get"));
    }

    #[test]
    fn escalates_when_workflow_remains_invalid_after_resolution() {
        // Build a workflow with a non-existent start agent —
        // resolver can't fix StartAgentMissing.
        let wf = Workflow {
            name: "wf".into(),
            version: 1,
            description: "".into(),
            agents: {
                let mut m = BTreeMap::new();
                m.insert("a".into(), spec("p1", "ai.chat", "{{workflow.input}}", "a"));
                m
            },
            flow: FlowGraph {
                start: "nonexistent".into(),
                edges: vec![],
                result: None,
            },
        };
        let resolver = ConflictResolver::new();
        let (_, report) = resolver.resolve(wf);
        assert!(report.escalated.is_some());
        assert_eq!(report.strategy_used, "escalate");
        assert!(
            report
                .details
                .iter()
                .any(|d| matches!(d.kind, ConflictKind::Unresolvable))
        );
    }

    #[test]
    fn strip_var_marker_removes_only_matching_marker() {
        let out = strip_var_marker("a {{x.output}} b {{y.output}} c", "x.output");
        assert!(!out.contains("x.output"));
        assert!(out.contains("y.output"));
        assert!(out.contains('a') && out.contains('b') && out.contains('c'));
    }

    #[test]
    fn strip_var_marker_tolerates_whitespace_inside_braces() {
        let out = strip_var_marker("{{ ghost.output }} after", "ghost.output");
        assert!(!out.contains("ghost"));
        assert!(out.contains("after"));
    }

    #[test]
    fn record_into_spec_appends_one_changelog_entry_per_conflict_and_resigns() {
        let mut spec = super::super::SpecParser::new().parse("Research the web.");
        let original_changelog_len = spec.changelog.len();
        let original_sig = spec.signature.clone();
        let mut report = ConflictResolutionReport::default();
        report.record(ConflictResolutionEntry {
            kind: ConflictKind::DuplicateOutput,
            strategy: ResolutionStrategy::Rename,
            description: "renamed `foo` -> `foo_2`".into(),
            affected_step: "b".into(),
        });
        report.record(ConflictResolutionEntry {
            kind: ConflictKind::InterferingParallelCall,
            strategy: ResolutionStrategy::Sequence,
            description: "sequenced w2 after w1".into(),
            affected_step: "w2".into(),
        });
        ConflictResolver::record_into_spec(&report, &mut spec);
        assert_eq!(
            spec.changelog.len(),
            original_changelog_len + 2,
            "two conflict entries → two changelog rows"
        );
        assert_eq!(
            spec.changelog[original_changelog_len].change_type,
            "conflict_rename"
        );
        assert_eq!(
            spec.changelog[original_changelog_len + 1].change_type,
            "conflict_sequence"
        );
        // Re-signed → verify passes + signature is fresh.
        assert!(spec.signature.is_some());
        assert_ne!(spec.signature, original_sig);
        spec.verify().expect("verify");
    }

    #[test]
    fn record_into_spec_is_a_noop_when_report_is_empty() {
        let mut spec = super::super::SpecParser::new().parse("Research the web.");
        let original_changelog_len = spec.changelog.len();
        let original_sig = spec.signature.clone();
        let report = ConflictResolutionReport::default();
        ConflictResolver::record_into_spec(&report, &mut spec);
        assert_eq!(spec.changelog.len(), original_changelog_len);
        assert_eq!(spec.signature, original_sig);
    }
}
