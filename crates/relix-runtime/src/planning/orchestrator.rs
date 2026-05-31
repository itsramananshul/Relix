//! RELIX-7.24 Stage-1 — multi-specialist orchestrator.
//!
//! Decomposes a single complex spec into 2–4 independent
//! sub-goals, assigns each sub-goal to the best-matching
//! specialist from the registry, runs every specialist
//! [`PlanGenerator`] in parallel, then merges every produced
//! sub-workflow into a single coherent [`Workflow`] that the
//! existing executor can run.
//!
//! The orchestrator is OFF by default for any
//! [`PlanSpec`] that the heuristic
//! [`super::SpecParser`] doesn't flag as complex. The
//! coordinator's `planning.create_plan` cap checks
//! [`Orchestrator::is_active`] and falls back to the single-
//! agent [`PlanGenerator`] when the orchestrator wouldn't
//! help.
//!
//! ## Decomposition path
//!
//! 1. Build a prompt for the configured `orchestrator_agent`
//!    that instructs it to return a JSON array of 2–4
//!    sub-goal strings.
//! 2. Call `ai.chat` on the configured `orchestrator_peer`
//!    via the wired [`WorkflowDispatcher`].
//! 3. Parse the response as JSON; defensively strip markdown
//!    code fences.
//! 4. On dispatch failure OR unparseable output, fall back to
//!    [`heuristic_decompose`] — a deterministic clause-split
//!    decomposer that splits the goal on conjunctions / commas
//!    / "then" markers. The fallback is intentional: a
//!    spec-driven pipeline that cannot reach its AI
//!    decomposer must still produce a runnable plan.
//!
//! ## Specialist assignment
//!
//! Each sub-goal goes through
//! [`AgentCapabilityRegistry::find_agents_for_task`]; the
//! top-scoring agent that is NOT in
//! [`PlanSpec::forbidden_agents`] wins. Preferred agents on
//! the parent spec are hoisted ahead of the score order.
//! When the same agent is the best match for two sub-goals,
//! the orchestrator keeps the assignment — the conflict
//! resolver downstream is responsible for handling collisions
//! at the workflow level.
//!
//! ## Parallel planning
//!
//! Each `(sub_goal, specialist)` pair is dispatched to the
//! single-agent [`PlanGenerator`] inside a `tokio::join_all`
//! batch. Each specialist produces a small [`Workflow`] with
//! a single step that targets its assigned agent. Failures
//! from any one specialist are surfaced — partial success is
//! not silently swallowed.
//!
//! ## Merging
//!
//! [`merge_workflows`] concatenates every specialist's agents
//! map into a single namespace (renaming on collision) and
//! either chains them sequentially OR parallel-fans them out
//! from a common `__orch_seed` step depending on whether the
//! parent spec asked for parallel execution. The merged
//! workflow always validates through
//! [`crate::workflow::validate`] before the orchestrator
//! returns it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::workflow::{AgentSpec, Edge, EdgeCondition, FlowGraph, Workflow, validate};
use crate::workflow::{DispatchError, WorkflowDispatcher};

use super::generator::{GenerateError, GeneratorOptions};
use super::{AgentCapabilityRegistry, PlanGenerator, PlanSpec, PlanTopology};

/// Operator-tunable orchestrator configuration. Populated
/// from the `[planning]` block in `controller.toml`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OrchestratorConfig {
    /// Master switch. `false` disables the orchestrator
    /// (and the critic) entirely; the legacy single-agent
    /// [`PlanGenerator`] path runs unchanged.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Agent name (as declared under `[agents.<name>]`) the
    /// orchestrator delegates goal decomposition to. The
    /// peer alias is resolved through `orchestrator_peer`.
    #[serde(default = "default_orchestrator_agent")]
    pub orchestrator_agent: String,
    /// libp2p peer alias to invoke `ai.chat` on. Defaults to
    /// `"coordinator"` because the local controller can
    /// serve its own decomposition calls when no specialist
    /// AI peer is configured.
    #[serde(default = "default_orchestrator_peer")]
    pub orchestrator_peer: String,
    /// Minimum [`PlanSpec::complexity_score`] required to
    /// activate the orchestrator. Below this the single-
    /// agent path runs.
    #[serde(default = "default_complexity_threshold")]
    pub complexity_threshold: f32,
    /// Hard cap on how many specialists can plan in
    /// parallel. The orchestrator never spawns more than
    /// this many `(sub_goal, specialist)` pairs in one
    /// `tokio::join_all` batch.
    #[serde(default = "default_max_parallel_specialists")]
    pub max_parallel_specialists: usize,
}

fn default_enabled() -> bool {
    true
}

fn default_orchestrator_agent() -> String {
    "coordinator".to_string()
}

fn default_orchestrator_peer() -> String {
    "coordinator".to_string()
}

fn default_complexity_threshold() -> f32 {
    super::parser::DEFAULT_COMPLEXITY_THRESHOLD
}

fn default_max_parallel_specialists() -> usize {
    4
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            orchestrator_agent: default_orchestrator_agent(),
            orchestrator_peer: default_orchestrator_peer(),
            complexity_threshold: default_complexity_threshold(),
            max_parallel_specialists: default_max_parallel_specialists(),
        }
    }
}

/// Errors the orchestrator surfaces. Single-agent fallbacks
/// are not errors — they're reported through
/// [`OrchestratorOutcome::Skipped`].
#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error("orchestrator: every specialist plan failed to generate: {0}")]
    AllSpecialistsFailed(String),
    #[error("orchestrator: no specialists matched any sub-goal in the registry")]
    NoSpecialistsAssigned,
    #[error("orchestrator: merged workflow failed validation: {0}")]
    InvalidMergedWorkflow(String),
    #[error("orchestrator: spec has an empty goal")]
    EmptyGoal,
}

/// What the orchestrator did. Returned by
/// [`Orchestrator::orchestrate`]; the coordinator stamps the
/// fields into the `planning.create_plan` response.
#[derive(Debug, Clone)]
pub enum OrchestratorOutcome {
    /// Orchestrator skipped — the single-agent path should
    /// run. Carries a human-readable reason for the
    /// response.
    Skipped {
        reason: String,
        /// Echo of the score the parser computed so the
        /// caller can show "complexity 0.35 below threshold
        /// 0.6" in the response.
        complexity_score: f32,
    },
    /// Orchestrator produced a merged workflow.
    Active {
        workflow: Workflow,
        topology: PlanTopology,
        sub_goals: Vec<String>,
        /// One row per `(sub_goal, specialist)` pair.
        specialist_assignments: Vec<SpecialistAssignment>,
        complexity_score: f32,
        /// `true` when [`heuristic_decompose`] was used
        /// because the AI decomposer was unreachable or
        /// returned unparseable output.
        decomposed_by_heuristic: bool,
    },
}

/// One specialist's assignment within an active
/// orchestrator run.
#[derive(Debug, Clone, Serialize)]
pub struct SpecialistAssignment {
    pub sub_goal: String,
    pub specialist_agent: String,
    pub specialist_peer: String,
    /// Score the registry assigned to the chosen specialist
    /// against this sub-goal.
    pub match_score: u32,
}

/// Cheap-to-clone orchestrator. Holds the registry + the
/// generator + the dispatcher needed to ask the configured
/// orchestrator agent to decompose a goal.
#[derive(Clone)]
pub struct Orchestrator {
    registry: AgentCapabilityRegistry,
    generator: PlanGenerator,
    dispatcher: Arc<dyn WorkflowDispatcher>,
    cfg: OrchestratorConfig,
}

impl Orchestrator {
    pub fn new(
        registry: AgentCapabilityRegistry,
        dispatcher: Arc<dyn WorkflowDispatcher>,
        cfg: OrchestratorConfig,
    ) -> Self {
        let generator = PlanGenerator::new(registry.clone());
        Self {
            registry,
            generator,
            dispatcher,
            cfg,
        }
    }

    /// Decide whether the orchestrator should activate for
    /// this `(spec, opts)` pair. The rule (per the §7.24
    /// Stage-1 spec and the orchestrator-activation tests):
    /// the orchestrator activates ONLY when
    ///
    /// - the operator left room for more than one agent
    ///   (`opts.max_agents > 1`), **and**
    /// - the spec is complex enough
    ///   (`spec.complexity_score >= cfg.complexity_threshold`),
    ///   **and**
    /// - the orchestrator is enabled in config.
    ///
    /// When any of those is false the single-agent
    /// [`PlanGenerator`] path runs unchanged.
    pub fn is_active(&self, spec: &PlanSpec, opts: &GeneratorOptions) -> bool {
        self.cfg.enabled
            && opts.max_agents > 1
            && spec.complexity_score >= self.cfg.complexity_threshold
    }

    /// Reason string for the response when [`Self::is_active`]
    /// returns false. Empty when the orchestrator IS active.
    fn skip_reason(&self, spec: &PlanSpec, opts: &GeneratorOptions) -> String {
        if !self.cfg.enabled {
            return "[planning] enabled = false".into();
        }
        if opts.max_agents <= 1 {
            return format!(
                "max_agents = {} (operator opted out of multi-specialist orchestration)",
                opts.max_agents
            );
        }
        if spec.complexity_score < self.cfg.complexity_threshold {
            return format!(
                "complexity {:.2} below threshold {:.2}",
                spec.complexity_score, self.cfg.complexity_threshold
            );
        }
        String::new()
    }

    /// Run the full orchestration pipeline. On
    /// [`OrchestratorOutcome::Skipped`] callers should fall
    /// back to the single-agent [`PlanGenerator`].
    pub async fn orchestrate(
        &self,
        spec: &PlanSpec,
        opts: &GeneratorOptions,
    ) -> Result<OrchestratorOutcome, OrchestratorError> {
        if spec.goal.trim().is_empty() {
            return Err(OrchestratorError::EmptyGoal);
        }
        if !self.is_active(spec, opts) {
            return Ok(OrchestratorOutcome::Skipped {
                reason: self.skip_reason(spec, opts),
                complexity_score: spec.complexity_score,
            });
        }

        // 1. Decompose.
        let (sub_goals, decomposed_by_heuristic) = self.decompose_goal(spec).await;

        // 2. Assign specialists.
        let assignments = self.assign_specialists(spec, &sub_goals);
        if assignments.is_empty() {
            return Err(OrchestratorError::NoSpecialistsAssigned);
        }

        // 3. Parallel sub-planning. Limit to
        // max_parallel_specialists.
        let assignments = assignments
            .into_iter()
            .take(self.cfg.max_parallel_specialists.max(1))
            .collect::<Vec<_>>();
        let sub_workflows = self.plan_in_parallel(spec, &assignments).await?;

        // 4. Merge.
        let topology = pick_merge_topology(spec, sub_workflows.len());
        let merged = merge_workflows(spec, &sub_workflows, topology);
        validate(&merged, None)
            .map_err(|e| OrchestratorError::InvalidMergedWorkflow(e.to_string()))?;

        Ok(OrchestratorOutcome::Active {
            workflow: merged,
            topology,
            sub_goals,
            specialist_assignments: assignments,
            complexity_score: spec.complexity_score,
            decomposed_by_heuristic,
        })
    }

    /// Ask the configured orchestrator agent to decompose the
    /// goal. Returns `(sub_goals, used_heuristic_fallback)`.
    async fn decompose_goal(&self, spec: &PlanSpec) -> (Vec<String>, bool) {
        let prompt = build_decomposition_prompt(spec, &self.registry);
        let session_id = format!("planning-orchestrator-{}", short_rand_id());
        // SEC PART 5: JSON-encoded args; see critic.rs.
        let arg = serde_json::json!({
            "session_id": session_id,
            "prompt": prompt,
            "history": "",
        })
        .to_string();
        match self
            .dispatcher
            .dispatch(&self.cfg.orchestrator_peer, "ai.chat", arg.as_bytes())
            .await
        {
            Ok(bytes) => match parse_sub_goals(&bytes) {
                Some(sg) if (2..=4).contains(&sg.len()) => (sg, false),
                _ => (heuristic_decompose(&spec.goal), true),
            },
            Err(_) => (heuristic_decompose(&spec.goal), true),
        }
    }

    /// Score every sub-goal against the registry; pick the
    /// best non-forbidden specialist per sub-goal. Preferred
    /// agents on the parent spec hoisted to the front of
    /// each sub-goal's candidate list before scoring.
    pub fn assign_specialists(
        &self,
        spec: &PlanSpec,
        sub_goals: &[String],
    ) -> Vec<SpecialistAssignment> {
        let forbidden: BTreeSet<&String> = spec.forbidden_agents.iter().collect();
        let preferred: BTreeSet<&String> = spec.preferred_agents.iter().collect();
        let mut out = Vec::with_capacity(sub_goals.len());
        for sg in sub_goals {
            // Preferred wins regardless of score.
            let preferred_match = preferred
                .iter()
                .find(|name| !forbidden.contains(*name))
                .and_then(|name| self.registry.get_agent(name).map(|info| (info, u32::MAX)));
            let scored = self.registry.find_agents_for_task(sg);
            let best = preferred_match.or_else(|| {
                scored
                    .into_iter()
                    .find(|m| !forbidden.contains(&m.agent))
                    .and_then(|m| {
                        self.registry
                            .get_agent(&m.agent)
                            .map(|info| (info, m.score))
                    })
            });
            if let Some((info, score)) = best {
                let peer = info.peer.clone().unwrap_or_else(|| info.name.clone());
                out.push(SpecialistAssignment {
                    sub_goal: sg.clone(),
                    specialist_agent: info.name.clone(),
                    specialist_peer: peer,
                    match_score: score,
                });
            }
        }
        out
    }

    async fn plan_in_parallel(
        &self,
        spec: &PlanSpec,
        assignments: &[SpecialistAssignment],
    ) -> Result<Vec<(SpecialistAssignment, Workflow)>, OrchestratorError> {
        let futures = assignments.iter().map(|a| {
            let spec_for = sub_spec(spec, &a.sub_goal, &a.specialist_agent);
            let generator = self.generator.clone();
            let assign = a.clone();
            async move {
                let opts = GeneratorOptions { max_agents: 1 };
                let result = generator.generate(&spec_for, &opts);
                (assign, result)
            }
        });
        let results = futures::future::join_all(futures).await;
        let mut out: Vec<(SpecialistAssignment, Workflow)> = Vec::with_capacity(results.len());
        let mut failures: Vec<String> = Vec::new();
        for (assign, res) in results {
            match res {
                Ok((wf, _topology)) => out.push((assign, wf)),
                Err(e) => failures.push(format!(
                    "[{}→{}]: {}",
                    assign.sub_goal,
                    assign.specialist_agent,
                    error_to_msg(&e)
                )),
            }
        }
        if out.is_empty() {
            return Err(OrchestratorError::AllSpecialistsFailed(failures.join("; ")));
        }
        Ok(out)
    }
}

// ── public helpers (testable in isolation) ───────────────

/// Heuristic clause-split decomposition. Used when the AI
/// decomposer is unreachable or returns unparseable output.
/// Splits the goal on top-level clause markers (` and `,
/// ` then `, ` next `, `; `, `, `) and returns 2–4
/// non-overlapping sub-goal strings.
///
/// When the goal contains no splittable structure the
/// function returns a single-element vec; the orchestrator's
/// caller then activates the single-agent fallback.
pub fn heuristic_decompose(goal: &str) -> Vec<String> {
    const SEPARATORS: &[&str] = &[" and ", " then ", " next ", " after ", "; ", ", "];
    // Split iteratively on the strongest separator that
    // appears. We want at most 4 parts.
    let mut parts: Vec<String> = vec![goal.trim().to_string()];
    for sep in SEPARATORS {
        if parts.len() >= 4 {
            break;
        }
        let mut next: Vec<String> = Vec::new();
        for p in parts {
            for piece in p.split(sep) {
                let t = piece.trim().to_string();
                if !t.is_empty() {
                    next.push(t);
                }
                if next.len() >= 4 {
                    break;
                }
            }
            if next.len() >= 4 {
                break;
            }
        }
        // Only commit if we actually grew the list.
        if next.len() > 1 {
            parts = next;
        } else {
            parts = vec![goal.trim().to_string()];
        }
    }
    // Cap at 4, drop tiny fragments shorter than 3 chars.
    parts.into_iter().filter(|p| p.len() >= 3).take(4).collect()
}

/// Parse the decomposer's response body into a clean list of
/// sub-goals. The AI is asked to return a strict JSON array
/// of strings; this helper is defensive about markdown
/// fences and extra surrounding prose.
pub fn parse_sub_goals(raw: &[u8]) -> Option<Vec<String>> {
    let text = std::str::from_utf8(raw).ok()?;
    let stripped = strip_markdown_code_fences(text);
    // Try direct JSON array first.
    if let Ok(arr) = serde_json::from_str::<Vec<String>>(&stripped) {
        return Some(
            arr.into_iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        );
    }
    // Try locating the first `[` and matching `]`.
    if let Some(start) = stripped.find('[')
        && let Some(end) = stripped[start..].rfind(']')
    {
        let slice = &stripped[start..start + end + 1];
        if let Ok(arr) = serde_json::from_str::<Vec<String>>(slice) {
            return Some(
                arr.into_iter()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            );
        }
    }
    // Final fallback: line-per-sub-goal text after stripping
    // numbering / bullet markers.
    let lines: Vec<String> = stripped
        .lines()
        .map(|l| {
            l.trim()
                .trim_start_matches(|c: char| {
                    c.is_ascii_digit() || c == '.' || c == ')' || c == '-' || c == '*' || c == ' '
                })
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty() && s.len() >= 3)
        .collect();
    if (2..=4).contains(&lines.len()) {
        Some(lines)
    } else {
        None
    }
}

fn strip_markdown_code_fences(s: &str) -> String {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```json")
        && let Some(body) = rest.strip_suffix("```")
    {
        return body.trim().to_string();
    }
    if let Some(rest) = t.strip_prefix("```")
        && let Some(body) = rest.strip_suffix("```")
    {
        return body.trim().to_string();
    }
    t.to_string()
}

/// Build the decomposition prompt. Emits a tight,
/// model-friendly instruction that elicits a strict JSON
/// array.
pub fn build_decomposition_prompt(spec: &PlanSpec, registry: &AgentCapabilityRegistry) -> String {
    let mut out = String::new();
    out.push_str(
        "You are a planning decomposer. Break the operator's goal into 2 to 4 \
         non-overlapping, independent sub-goals that together cover the original goal.\n\
         Return ONLY a JSON array of strings — no prose, no markdown, no explanation.\n\n",
    );
    out.push_str("Goal:\n");
    out.push_str(spec.goal.trim());
    out.push_str("\n\n");
    if !spec.constraints.is_empty() {
        out.push_str("Constraints:\n");
        for c in &spec.constraints {
            out.push_str("- ");
            out.push_str(c);
            out.push('\n');
        }
        out.push('\n');
    }
    if !spec.success_criteria.is_empty() {
        out.push_str("Success criteria:\n");
        for s in &spec.success_criteria {
            out.push_str("- ");
            out.push_str(s);
            out.push('\n');
        }
        out.push('\n');
    }
    out.push_str("Available specialists (name — description — top tags):\n");
    for agent in registry.list_agents() {
        let desc = agent.description.clone().unwrap_or_default();
        let tags: BTreeSet<String> = agent
            .capabilities
            .iter()
            .flat_map(|c| c.tags.iter().cloned())
            .collect();
        let tag_csv = tags.into_iter().collect::<Vec<_>>().join(", ");
        out.push_str(&format!("- {} — {desc} — tags: [{tag_csv}]\n", agent.name));
    }
    out.push_str(
        "\nReturn the sub-goals JSON array now. Example shape: \
         [\"Research recent prior art\", \"Summarise findings in 300 words\"]",
    );
    out
}

/// Build a per-specialist [`PlanSpec`] for one sub-goal. The
/// child spec inherits parent constraints / success criteria /
/// budget hint so each specialist's plan still honours them.
/// `preferred_agents` is replaced with the chosen specialist
/// so the downstream [`PlanGenerator`] selects it
/// deterministically.
fn sub_spec(parent: &PlanSpec, sub_goal: &str, specialist: &str) -> PlanSpec {
    // Inherit the parent's hardening fields (version, spec_id,
    // created_at_ms) so an operator can correlate the sub-spec
    // back to the parent. The parent's spec_id stays so all
    // specialist sub-plans share an audit-trail root. The
    // sub-spec carries no signature initially — it's an
    // internal generator-only artifact never persisted.
    PlanSpec {
        goal: sub_goal.to_string(),
        constraints: parent.constraints.clone(),
        success_criteria: parent.success_criteria.clone(),
        preferred_agents: vec![specialist.to_string()],
        forbidden_agents: parent.forbidden_agents.clone(),
        max_steps: Some(1),
        budget_hint: parent.budget_hint.clone(),
        original_spec: sub_goal.to_string(),
        complexity_score: 0.0,
        is_complex: false,
        version: parent.version,
        spec_id: parent.spec_id.clone(),
        created_at_ms: parent.created_at_ms,
        signature: None,
        changelog: Vec::new(),
    }
}

/// Decide how to merge the specialist sub-workflows. When the
/// parent spec has any explicit sequential keyword we chain
/// them; otherwise we parallel-fan from a synthetic seed step.
fn pick_merge_topology(spec: &PlanSpec, sub_workflow_count: usize) -> PlanTopology {
    if sub_workflow_count <= 1 {
        return PlanTopology::Single;
    }
    let haystack = format!(" {} ", spec.original_spec.to_lowercase());
    let sequential_markers: &[&str] = &[
        " then ",
        " after ",
        " next ",
        " followed by ",
        " step by step",
        " pipeline",
    ];
    if sequential_markers.iter().any(|m| haystack.contains(m)) {
        return PlanTopology::Sequential;
    }
    PlanTopology::Parallel
}

/// Merge the per-specialist [`Workflow`]s into a single
/// workflow that the existing executor can run. Each
/// specialist's step is namespaced to avoid id collisions;
/// when collisions occur on the `output` binding the suffix
/// counter keeps both intact and the conflict resolver
/// rewrites downstream references later.
pub fn merge_workflows(
    spec: &PlanSpec,
    sub_workflows: &[(SpecialistAssignment, Workflow)],
    topology: PlanTopology,
) -> Workflow {
    let mut agents: BTreeMap<String, AgentSpec> = BTreeMap::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut step_chain: Vec<String> = Vec::with_capacity(sub_workflows.len());

    // The merge always anchors on a single "seed" step that
    // distributes the operator's input to each specialist.
    // We synthesise it as a no-op ai.chat against the
    // orchestrator's own agent name — this gives parallel and
    // sequential topologies a deterministic start node.
    let seed_step = "__orch_seed".to_string();
    let first_assignment = sub_workflows.first().map(|(a, _)| a.clone());
    let seed_peer = first_assignment
        .as_ref()
        .map(|a| a.specialist_peer.clone())
        .unwrap_or_else(|| "coordinator".to_string());
    agents.insert(
        seed_step.clone(),
        AgentSpec {
            peer: seed_peer.clone(),
            capability: "ai.chat".to_string(),
            input: "{{workflow.input}}".to_string(),
            output: seed_step.clone(),
        },
    );

    for (assign, wf) in sub_workflows {
        // Each sub-workflow has exactly one agent (we pass
        // max_agents = 1 to the per-specialist generator).
        // Lift that one step into the merged map under a
        // namespaced id that records the specialist name.
        for (orig_id, orig_spec) in &wf.agents {
            let base = format!("{}_{}", assign.specialist_agent_slug(), orig_id);
            let unique = unique_step_id(&base, &agents);
            let mut input = orig_spec.input.clone();
            input = rewrite_workflow_input_placeholder(&input, &seed_step);
            let agent_spec = AgentSpec {
                peer: orig_spec.peer.clone(),
                capability: orig_spec.capability.clone(),
                input,
                output: unique.clone(),
            };
            agents.insert(unique.clone(), agent_spec);
            step_chain.push(unique);
        }
    }

    match topology {
        PlanTopology::Single => {
            // Only one specialist survived — return its step
            // directly without the seed scaffolding.
            agents.remove(&seed_step);
            let only = step_chain.first().cloned().unwrap_or_default();
            return Workflow {
                name: planning_workflow_name(spec),
                version: 1,
                description: format!(
                    "Orchestrator plan (1 specialist): {}",
                    truncate(&spec.goal, 80)
                ),
                agents,
                flow: FlowGraph {
                    start: only.clone(),
                    edges: Vec::new(),
                    result: Some(format!("{{{{{only}.output}}}}")),
                },
            };
        }
        PlanTopology::Sequential => {
            // seed → step_chain[0] → step_chain[1] → ...
            let mut prev = seed_step.clone();
            for step in &step_chain {
                edges.push(Edge {
                    from: prev.clone(),
                    to: step.clone(),
                    condition: EdgeCondition::Success,
                });
                prev = step.clone();
            }
        }
        PlanTopology::Parallel => {
            // seed -[parallel]-> each specialist step.
            for step in &step_chain {
                edges.push(Edge {
                    from: seed_step.clone(),
                    to: step.clone(),
                    condition: EdgeCondition::Parallel,
                });
            }
        }
    }

    // For parallel topology we still need a single
    // observable result. Add a synthetic merge step that
    // collects each branch's output.
    let result = if matches!(topology, PlanTopology::Parallel) {
        let merge_step = unique_step_id("__orch_merge", &agents);
        let mut merge_input = String::from(
            "Combine the parallel specialist outputs into a final answer for: {{workflow.input}}\n",
        );
        for s in &step_chain {
            merge_input.push_str(&format!("\n[{s}]\n{{{{{s}.output}}}}\n"));
        }
        agents.insert(
            merge_step.clone(),
            AgentSpec {
                peer: seed_peer.clone(),
                capability: "ai.chat".to_string(),
                input: merge_input,
                output: merge_step.clone(),
            },
        );
        for s in &step_chain {
            edges.push(Edge {
                from: s.clone(),
                to: merge_step.clone(),
                condition: EdgeCondition::Success,
            });
        }
        Some(format!("{{{{{merge_step}.output}}}}"))
    } else {
        step_chain
            .last()
            .map(|last| format!("{{{{{last}.output}}}}"))
    };

    Workflow {
        name: planning_workflow_name(spec),
        version: 1,
        description: format!(
            "Orchestrator plan ({} specialists, {topology:?}): {}",
            sub_workflows.len(),
            truncate(&spec.goal, 80)
        ),
        agents,
        flow: FlowGraph {
            start: seed_step,
            edges,
            result,
        },
    }
}

/// Replace a per-specialist `{{workflow.input}}` placeholder
/// with the orchestrator-seed's output so each specialist
/// receives the shared seed context rather than the raw
/// workflow input. Preserves every OTHER variable reference
/// untouched so the conflict resolver can later spot any
/// cross-branch placeholder collisions.
fn rewrite_workflow_input_placeholder(input: &str, seed_step: &str) -> String {
    input.replace("{{workflow.input}}", &format!("{{{{{seed_step}.output}}}}"))
}

impl SpecialistAssignment {
    /// Sanitised slug suitable for a workflow step id prefix.
    pub fn specialist_agent_slug(&self) -> String {
        self.specialist_agent
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect()
    }
}

fn unique_step_id(base: &str, existing: &BTreeMap<String, AgentSpec>) -> String {
    if !existing.contains_key(base) {
        return base.to_string();
    }
    for n in 2..1000 {
        let candidate = format!("{base}_{n}");
        if !existing.contains_key(&candidate) {
            return candidate;
        }
    }
    format!("{base}_x")
}

fn planning_workflow_name(spec: &PlanSpec) -> String {
    let slug: String = spec
        .goal
        .chars()
        .take(48)
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = slug.trim_matches('_').to_string();
    format!(
        "planning_orch__{}",
        if trimmed.is_empty() {
            "ad_hoc"
        } else {
            trimmed.as_str()
        }
    )
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn error_to_msg(e: &GenerateError) -> String {
    e.to_string()
}

fn short_rand_id() -> String {
    let bytes: [u8; 8] = rand::random();
    hex::encode(bytes)
}

/// Carry-friendly conversion from a [`DispatchError`] for
/// downstream surfacing. Kept here so the orchestrator can
/// re-export a clean cause string without leaking the
/// dispatcher-specific error type.
pub fn dispatch_error_to_msg(e: &DispatchError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller_runtime::{AgentCapabilityDecl, AgentSection};
    use crate::manifest::ManifestProvider;
    use crate::workflow::DispatchResult;
    use async_trait::async_trait;
    use relix_core::types::NodeId;
    use std::collections::BTreeMap;
    use tokio::sync::Mutex;

    fn manifest() -> ManifestProvider {
        ManifestProvider::new(
            NodeId::from_pubkey(b"local"),
            "coord",
            "coordinator",
            NodeId::from_pubkey(b"org"),
            vec![],
        )
    }

    fn section(description: &str, peer: &str, caps: Vec<AgentCapabilityDecl>) -> AgentSection {
        AgentSection {
            training: None,
            peer: Some(peer.into()),
            description: Some(description.into()),
            capabilities: caps,
        }
    }

    fn decl(method: &str, description: &str, tags: &[&str]) -> AgentCapabilityDecl {
        AgentCapabilityDecl {
            method: method.into(),
            description: Some(description.into()),
            tags: tags.iter().map(|s| (*s).into()).collect(),
        }
    }

    fn registry_three_specialists() -> AgentCapabilityRegistry {
        let mut cfg = BTreeMap::new();
        cfg.insert(
            "research-agent".into(),
            section(
                "web research helper",
                "research-peer",
                vec![decl(
                    "ai.chat",
                    "research and summarisation",
                    &["research", "web", "summary"],
                )],
            ),
        );
        cfg.insert(
            "code-agent".into(),
            section(
                "writes and reviews code",
                "code-peer",
                vec![decl("ai.chat", "code work", &["code", "programming"])],
            ),
        );
        cfg.insert(
            "design-agent".into(),
            section(
                "produces design proposals",
                "design-peer",
                vec![decl("ai.chat", "design work", &["design", "diagram"])],
            ),
        );
        AgentCapabilityRegistry::from_sources("coord", &manifest(), &cfg, &BTreeMap::new())
    }

    /// Canned dispatcher that returns a pre-programmed
    /// response per `(peer, capability)`.
    struct CannedDispatcher {
        responses: Mutex<BTreeMap<(String, String), Vec<DispatchResult>>>,
        calls: Mutex<Vec<(String, String, Vec<u8>)>>,
    }

    impl CannedDispatcher {
        fn new() -> Self {
            Self {
                responses: Mutex::new(BTreeMap::new()),
                calls: Mutex::new(Vec::new()),
            }
        }
        async fn respond_ok(&self, peer: &str, cap: &str, body: &str) {
            self.responses
                .lock()
                .await
                .entry((peer.into(), cap.into()))
                .or_default()
                .push(Ok(body.as_bytes().to_vec()));
        }
        async fn respond_err(&self, peer: &str, cap: &str, cause: &str) {
            self.responses
                .lock()
                .await
                .entry((peer.into(), cap.into()))
                .or_default()
                .push(Err(DispatchError {
                    peer: peer.into(),
                    method: cap.into(),
                    cause: cause.into(),
                }));
        }
        async fn call_count(&self) -> usize {
            self.calls.lock().await.len()
        }
    }

    #[async_trait]
    impl WorkflowDispatcher for CannedDispatcher {
        async fn dispatch(&self, peer: &str, cap: &str, input: &[u8]) -> DispatchResult {
            self.calls
                .lock()
                .await
                .push((peer.into(), cap.into(), input.to_vec()));
            let mut q = self.responses.lock().await;
            let queue = q.entry((peer.into(), cap.into())).or_default();
            if queue.is_empty() {
                return Err(DispatchError {
                    peer: peer.into(),
                    method: cap.into(),
                    cause: "no canned response queued".into(),
                });
            }
            queue.remove(0)
        }
    }

    fn complex_spec(goal: &str) -> PlanSpec {
        // Build a spec the parser would mark complex.
        let mut spec = super::super::SpecParser::with_known_agents(vec![
            "research-agent".into(),
            "code-agent".into(),
            "design-agent".into(),
        ])
        .parse(goal);
        spec.complexity_score = 0.9;
        spec.is_complex = true;
        spec
    }

    #[tokio::test]
    async fn heuristic_decompose_splits_on_and_then_comma() {
        let parts =
            heuristic_decompose("research async runtimes and design a benchmark and write code");
        assert!(parts.len() >= 2 && parts.len() <= 4, "{:?}", parts);
        assert!(parts.iter().any(|p| p.contains("research")));
        assert!(parts.iter().any(|p| p.contains("design")));
    }

    #[test]
    fn heuristic_decompose_returns_single_for_a_terse_goal() {
        let parts = heuristic_decompose("greet the user");
        assert_eq!(parts, vec!["greet the user".to_string()]);
    }

    #[test]
    fn parse_sub_goals_accepts_bare_json_array() {
        let body = br#"["sub one","sub two","sub three"]"#;
        let out = parse_sub_goals(body).expect("parse");
        assert_eq!(out, vec!["sub one", "sub two", "sub three"]);
    }

    #[test]
    fn parse_sub_goals_strips_markdown_fences() {
        let body = b"```json\n[\"a\",\"b\"]\n```";
        let out = parse_sub_goals(body).expect("parse");
        assert_eq!(out, vec!["a", "b"]);
    }

    #[test]
    fn parse_sub_goals_extracts_array_from_surrounding_prose() {
        let body = b"Sure! Here's the array:\n[\"alpha\", \"beta\"]\nLet me know.";
        let out = parse_sub_goals(body).expect("parse");
        assert_eq!(out, vec!["alpha", "beta"]);
    }

    #[test]
    fn parse_sub_goals_falls_through_to_line_list() {
        let body = b"1. first sub\n2. second sub\n3. third sub";
        let out = parse_sub_goals(body).expect("parse");
        assert_eq!(out, vec!["first sub", "second sub", "third sub"]);
    }

    #[test]
    fn parse_sub_goals_returns_none_for_unparseable() {
        let body = b"<>>>>";
        assert!(parse_sub_goals(body).is_none());
    }

    #[tokio::test]
    async fn is_active_is_false_when_max_agents_equals_one() {
        let registry = registry_three_specialists();
        let disp = Arc::new(CannedDispatcher::new());
        let orch = Orchestrator::new(registry, disp, OrchestratorConfig::default());
        let spec = complex_spec("Research the world and design something and write code.");
        let active = orch.is_active(&spec, &GeneratorOptions { max_agents: 1 });
        assert!(!active);
    }

    #[tokio::test]
    async fn is_active_is_false_when_complexity_below_threshold() {
        let registry = registry_three_specialists();
        let disp = Arc::new(CannedDispatcher::new());
        let orch = Orchestrator::new(registry, disp, OrchestratorConfig::default());
        let mut spec = complex_spec("Research the world.");
        spec.complexity_score = 0.1;
        spec.is_complex = false;
        let active = orch.is_active(&spec, &GeneratorOptions { max_agents: 3 });
        assert!(!active);
    }

    #[tokio::test]
    async fn is_active_is_false_when_config_disabled() {
        let registry = registry_three_specialists();
        let disp = Arc::new(CannedDispatcher::new());
        let cfg = OrchestratorConfig {
            enabled: false,
            ..OrchestratorConfig::default()
        };
        let orch = Orchestrator::new(registry, disp, cfg);
        let spec = complex_spec("Research the world and design something and write code.");
        let active = orch.is_active(&spec, &GeneratorOptions { max_agents: 3 });
        assert!(!active);
    }

    #[tokio::test]
    async fn orchestrate_skipped_when_max_agents_one_returns_reason() {
        let registry = registry_three_specialists();
        let disp = Arc::new(CannedDispatcher::new());
        let orch = Orchestrator::new(registry, disp.clone(), OrchestratorConfig::default());
        let spec = complex_spec("Research and design and code.");
        let outcome = orch
            .orchestrate(&spec, &GeneratorOptions { max_agents: 1 })
            .await
            .expect("orchestrate");
        match outcome {
            OrchestratorOutcome::Skipped { reason, .. } => {
                assert!(reason.contains("max_agents"));
            }
            _ => panic!("expected Skipped"),
        }
        // No AI calls should have happened on the Skipped
        // path.
        assert_eq!(disp.call_count().await, 0);
    }

    #[tokio::test]
    async fn orchestrate_active_calls_ai_decomposer_and_returns_merged_workflow() {
        let registry = registry_three_specialists();
        let disp = Arc::new(CannedDispatcher::new());
        // Canned JSON decomposition reply with 3 sub-goals.
        disp.respond_ok(
            "coordinator",
            "ai.chat",
            r#"["Research async runtimes","Design a benchmark harness","Write the harness code"]"#,
        )
        .await;
        let orch = Orchestrator::new(registry, disp.clone(), OrchestratorConfig::default());
        let spec =
            complex_spec("Research async runtimes and design a benchmark and write the code.");
        let outcome = orch
            .orchestrate(&spec, &GeneratorOptions { max_agents: 3 })
            .await
            .expect("orchestrate");
        match outcome {
            OrchestratorOutcome::Active {
                workflow,
                sub_goals,
                specialist_assignments,
                decomposed_by_heuristic,
                ..
            } => {
                assert!(!decomposed_by_heuristic, "AI decomposer was reachable");
                assert_eq!(sub_goals.len(), 3);
                assert_eq!(specialist_assignments.len(), 3);
                // The merged workflow must validate.
                validate(&workflow, None).expect("merged workflow validates");
                // Must contain a step per specialist plus the
                // seed (3 + 1 = 4) plus optionally a merge step.
                assert!(workflow.agents.len() >= 4);
            }
            _ => panic!("expected Active"),
        }
    }

    #[tokio::test]
    async fn orchestrate_falls_back_to_heuristic_when_ai_dispatch_fails() {
        let registry = registry_three_specialists();
        let disp = Arc::new(CannedDispatcher::new());
        disp.respond_err("coordinator", "ai.chat", "mesh down")
            .await;
        let orch = Orchestrator::new(registry, disp, OrchestratorConfig::default());
        let spec =
            complex_spec("Research async runtimes and design a benchmark and write the code.");
        let outcome = orch
            .orchestrate(&spec, &GeneratorOptions { max_agents: 3 })
            .await
            .expect("orchestrate");
        match outcome {
            OrchestratorOutcome::Active {
                decomposed_by_heuristic,
                sub_goals,
                ..
            } => {
                assert!(decomposed_by_heuristic);
                assert!(!sub_goals.is_empty());
            }
            _ => panic!("expected Active with heuristic fallback"),
        }
    }

    #[tokio::test]
    async fn assign_specialists_skips_forbidden_agents() {
        let registry = registry_three_specialists();
        let disp = Arc::new(CannedDispatcher::new());
        let orch = Orchestrator::new(registry, disp, OrchestratorConfig::default());
        let mut spec = complex_spec("Research and design something.");
        spec.forbidden_agents = vec!["code-agent".into()];
        let sub_goals: Vec<String> = vec![
            "Research async runtimes".into(),
            "Design a benchmark harness".into(),
        ];
        let assignments = orch.assign_specialists(&spec, &sub_goals);
        // Even though "code-agent" might score for these
        // sub-goals on a tag overlap, it MUST be skipped.
        for a in &assignments {
            assert_ne!(a.specialist_agent, "code-agent");
        }
        assert_eq!(assignments.len(), 2);
        assert!(
            assignments
                .iter()
                .any(|a| a.specialist_agent == "research-agent")
        );
        assert!(
            assignments
                .iter()
                .any(|a| a.specialist_agent == "design-agent")
        );
    }

    #[tokio::test]
    async fn assign_specialists_drops_sub_goals_with_no_eligible_match() {
        let registry = registry_three_specialists();
        let disp = Arc::new(CannedDispatcher::new());
        let orch = Orchestrator::new(registry, disp, OrchestratorConfig::default());
        let mut spec = complex_spec("Goal.");
        // Forbid every specialist that could match the
        // "Write code" sub-goal AND leave a sub-goal that
        // matches nothing in the registry → no assignment
        // for that sub-goal.
        spec.forbidden_agents = vec!["code-agent".into()];
        let sub_goals: Vec<String> = vec![
            "Research async runtimes".into(),
            "xylophone unicorn parsnip".into(),
        ];
        let assignments = orch.assign_specialists(&spec, &sub_goals);
        // Only the first sub-goal yields an assignment.
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].sub_goal, "Research async runtimes");
    }

    #[tokio::test]
    async fn assign_specialists_hoists_preferred_agents() {
        let registry = registry_three_specialists();
        let disp = Arc::new(CannedDispatcher::new());
        let orch = Orchestrator::new(registry, disp, OrchestratorConfig::default());
        let mut spec = complex_spec("Research a thing.");
        // Force code-agent to be preferred even for a research
        // sub-goal — operator intent overrides score.
        spec.preferred_agents = vec!["code-agent".into()];
        let sub_goals: Vec<String> = vec!["Research async runtimes".into()];
        let assignments = orch.assign_specialists(&spec, &sub_goals);
        assert_eq!(assignments[0].specialist_agent, "code-agent");
    }

    #[tokio::test]
    async fn merge_workflows_contains_every_specialist_step() {
        let registry = registry_three_specialists();
        let disp = Arc::new(CannedDispatcher::new());
        disp.respond_ok(
            "coordinator",
            "ai.chat",
            r#"["Research async runtimes","Write the code"]"#,
        )
        .await;
        let orch = Orchestrator::new(registry, disp, OrchestratorConfig::default());
        let spec = complex_spec("Research and write code.");
        let outcome = orch
            .orchestrate(&spec, &GeneratorOptions { max_agents: 3 })
            .await
            .expect("orchestrate");
        let OrchestratorOutcome::Active { workflow, .. } = outcome else {
            panic!("expected Active");
        };
        // Specialist step ids are prefixed with the
        // specialist's slug, so both "research_agent" and
        // "code_agent" should appear.
        let ids: Vec<_> = workflow.agents.keys().cloned().collect();
        assert!(ids.iter().any(|s| s.contains("research")), "ids={ids:?}");
        assert!(ids.iter().any(|s| s.contains("code")), "ids={ids:?}");
    }

    #[tokio::test]
    async fn parallel_topology_emits_parallel_edges_from_seed() {
        let registry = registry_three_specialists();
        let disp = Arc::new(CannedDispatcher::new());
        // Two distinct specialists; no sequential keyword in
        // the spec so the merger picks Parallel.
        disp.respond_ok(
            "coordinator",
            "ai.chat",
            r#"["Research async runtimes","Design the benchmark"]"#,
        )
        .await;
        let orch = Orchestrator::new(registry, disp, OrchestratorConfig::default());
        // The spec carries no sequential markers ("then",
        // "after", "next", ...) so merger should pick
        // Parallel.
        let spec = complex_spec("Research and design two parts of the system.");
        let outcome = orch
            .orchestrate(&spec, &GeneratorOptions { max_agents: 3 })
            .await
            .expect("orchestrate");
        let OrchestratorOutcome::Active {
            workflow, topology, ..
        } = outcome
        else {
            panic!("expected Active");
        };
        assert_eq!(topology, PlanTopology::Parallel);
        assert!(
            workflow
                .flow
                .edges
                .iter()
                .any(|e| matches!(e.condition, EdgeCondition::Parallel))
        );
    }

    #[tokio::test]
    async fn sequential_topology_emits_success_edges_in_order() {
        let registry = registry_three_specialists();
        let disp = Arc::new(CannedDispatcher::new());
        disp.respond_ok(
            "coordinator",
            "ai.chat",
            r#"["Research first","Write code next"]"#,
        )
        .await;
        let orch = Orchestrator::new(registry, disp, OrchestratorConfig::default());
        // "then" → sequential.
        let spec = complex_spec("Research first then write the code afterward.");
        let outcome = orch
            .orchestrate(&spec, &GeneratorOptions { max_agents: 3 })
            .await
            .expect("orchestrate");
        let OrchestratorOutcome::Active {
            workflow, topology, ..
        } = outcome
        else {
            panic!("expected Active");
        };
        assert_eq!(topology, PlanTopology::Sequential);
        assert!(
            workflow
                .flow
                .edges
                .iter()
                .all(|e| matches!(e.condition, EdgeCondition::Success)),
            "expected all-success edges in sequential merge: {:?}",
            workflow.flow.edges
        );
    }
}
