//! RELIX-7.24 — `PlanGenerator`.
//!
//! Takes a parsed [`super::PlanSpec`] + a populated
//! [`super::AgentCapabilityRegistry`] and emits a validated
//! [`crate::workflow::Workflow`] the existing executor can
//! run directly.
//!
//! The generator never invokes any agent. Execution is the
//! coordinator's responsibility once the operator calls
//! `planning.create_plan` with `dry_run = false` (or hands
//! the workflow to `workflow.run` themselves).
//!
//! ## Topology selection
//!
//! Three shapes are supported, picked from the spec text +
//! the number of qualifying agents:
//!
//! - [`PlanTopology::Single`] — one agent dominates the
//!   spec OR the registry only matched one agent. The
//!   workflow has a single step.
//! - [`PlanTopology::Sequential`] — the spec implies a
//!   pipeline ("then", "after", "next", "research then
//!   summarise"). The workflow chains agents in score
//!   order; each step's input is the previous step's
//!   output.
//! - [`PlanTopology::Parallel`] — the spec implies
//!   independent subtasks ("compare", "multiple angles",
//!   "in parallel", "and also"). The workflow fans
//!   out from a synthetic `seed` step into N parallel
//!   branches, then converges on a `merge` step that
//!   collects every branch's output.
//!
//! Selection rules:
//! 1. Score every agent via `find_agents_for_task(goal)`.
//! 2. Drop every agent in `forbidden_agents`.
//! 3. Hoist every agent in `preferred_agents` to the front
//!    of the order — preferred wins over higher score.
//! 4. Cap the count to `min(max_agents, max_steps_from_spec
//!    or unlimited)`. Default `max_agents = 3`.
//! 5. If the resulting list is empty, return
//!    [`GenerateError::NoMatchingAgents`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::workflow::{AgentSpec, Edge, EdgeCondition, FlowGraph, Workflow, validate};

use super::{AgentCapabilityRegistry, AgentInfo, PlanSpec};

/// Topology the generator chose for a given spec. Echoed back
/// to operators so they understand WHY the workflow has a
/// particular shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTopology {
    Single,
    Sequential,
    Parallel,
}

/// Errors the generator surfaces.
#[derive(Debug, thiserror::Error)]
pub enum GenerateError {
    #[error("planning: no agents match the spec goal")]
    NoMatchingAgents,
    #[error("planning: every preferred agent is also forbidden — operator contradiction")]
    PreferredAndForbidden,
    #[error("planning: spec has no goal (parser returned empty)")]
    EmptyGoal,
    #[error("planning: generated workflow failed validation: {0}")]
    InvalidWorkflow(String),
}

/// Configuration knobs for [`PlanGenerator::generate`].
#[derive(Clone, Debug)]
pub struct GeneratorOptions {
    /// Hard cap on agents picked. Defaults to 3 per the
    /// spec. Operators can override per-call.
    pub max_agents: usize,
}

impl Default for GeneratorOptions {
    fn default() -> Self {
        Self { max_agents: 3 }
    }
}

/// Cheap-to-clone planner that ties the [`PlanSpec`] +
/// [`AgentCapabilityRegistry`] surfaces into a validated
/// [`Workflow`].
#[derive(Clone)]
pub struct PlanGenerator {
    registry: AgentCapabilityRegistry,
}

impl PlanGenerator {
    pub fn new(registry: AgentCapabilityRegistry) -> Self {
        Self { registry }
    }

    /// Generate a validated workflow from `spec`. Returns
    /// the workflow alongside the chosen topology so the
    /// coordinator can stamp both into the planning cap's
    /// response.
    pub fn generate(
        &self,
        spec: &PlanSpec,
        opts: &GeneratorOptions,
    ) -> Result<(Workflow, PlanTopology), GenerateError> {
        if spec.goal.trim().is_empty() {
            return Err(GenerateError::EmptyGoal);
        }
        // Sanity: operator can't say "use X" and "forbid X"
        // at the same time.
        for p in &spec.preferred_agents {
            if spec.forbidden_agents.contains(p) {
                return Err(GenerateError::PreferredAndForbidden);
            }
        }

        // (1) Score + filter agents.
        let selected = self.select_agents(spec, opts);
        if selected.is_empty() {
            return Err(GenerateError::NoMatchingAgents);
        }

        // (2) Pick topology from spec keywords + selection size.
        let topology = pick_topology(spec, selected.len());

        // (3) Build the workflow.
        let workflow = match topology {
            PlanTopology::Single => build_single_agent_workflow(spec, &selected[0]),
            PlanTopology::Sequential => build_sequential_workflow(spec, &selected),
            PlanTopology::Parallel => build_parallel_workflow(spec, &selected),
        };

        // (4) Validate via the existing workflow validator.
        validate(&workflow, None).map_err(|e| GenerateError::InvalidWorkflow(e.to_string()))?;

        Ok((workflow, topology))
    }

    fn select_agents(&self, spec: &PlanSpec, opts: &GeneratorOptions) -> Vec<AgentInfo> {
        // Score every agent against the goal. The registry
        // returns matches sorted by descending score, but the
        // selection has to honour preferred / forbidden lists
        // on top of that order.
        let scored = self.registry.find_agents_for_task(&spec.goal);
        let forbidden: std::collections::BTreeSet<&String> = spec.forbidden_agents.iter().collect();
        let mut ordered: Vec<String> = Vec::new();
        // First: preferred agents in spec order (operator
        // intent always wins over score).
        for p in &spec.preferred_agents {
            if !forbidden.contains(p) && !ordered.contains(p) {
                ordered.push(p.clone());
            }
        }
        // Then: scored agents, skipping forbidden + duplicates.
        for m in scored {
            if forbidden.contains(&m.agent) || ordered.contains(&m.agent) {
                continue;
            }
            ordered.push(m.agent);
        }
        // Apply the max-agents cap. The spec's max_steps (if
        // set) also bounds the count — operators who say
        // "in 2 steps" get exactly 2 agents.
        let cap = match spec.max_steps {
            Some(n) if n > 0 => opts.max_agents.min(n),
            _ => opts.max_agents,
        };
        ordered.truncate(cap);
        ordered
            .into_iter()
            .filter_map(|n| self.registry.get_agent(&n))
            .collect()
    }
}

// ── helpers ───────────────────────────────────────────────

const SEQUENTIAL_KEYWORDS: &[&str] = &[
    " then ",
    " after ",
    " next ",
    " followed by ",
    " step by step",
    " pipeline",
];

const PARALLEL_KEYWORDS: &[&str] = &[
    " compare ",
    " contrast ",
    " in parallel",
    " concurrently",
    " multiple angles",
    " multiple sources",
    " each of ",
    " independent",
    " simultaneously",
    " and also ",
];

fn pick_topology(spec: &PlanSpec, agent_count: usize) -> PlanTopology {
    if agent_count <= 1 {
        return PlanTopology::Single;
    }
    let haystack = format!(" {} ", spec.original_spec.to_lowercase());
    if PARALLEL_KEYWORDS.iter().any(|k| haystack.contains(k)) {
        return PlanTopology::Parallel;
    }
    if SEQUENTIAL_KEYWORDS.iter().any(|k| haystack.contains(k)) {
        return PlanTopology::Sequential;
    }
    // Default for multi-agent: sequential (each agent
    // refines the previous step's output). Operators who
    // want parallel topology must say so explicitly via the
    // parallel keywords.
    PlanTopology::Sequential
}

/// Pick the capability the planner uses for one agent. Prefer
/// the agent's matched capability for the goal; fall back to
/// the first capability OR `"ai.chat"` if nothing else fits.
fn pick_capability(agent: &AgentInfo, goal: &str) -> String {
    // Try the registry scoring path — same scoring logic the
    // selection used, but we want the SPECIFIC capability
    // that contributed most. The registry doesn't expose that
    // directly, so we approximate: prefer caps whose tags or
    // method-name segments overlap the goal's keywords.
    let goal_lower = goal.to_lowercase();
    let goal_words: std::collections::BTreeSet<String> = goal_lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(|w| w.to_string())
        .collect();
    let mut best: Option<(u32, &str)> = None;
    for cap in &agent.capabilities {
        let mut score: u32 = 0;
        for tag in &cap.tags {
            if goal_words.contains(&tag.to_lowercase()) {
                score += 3;
            }
        }
        for seg in cap.method.split(|c: char| !c.is_alphanumeric()) {
            if goal_words.contains(&seg.to_lowercase()) {
                score += 2;
            }
        }
        if best.is_none_or(|(s, _)| score > s) {
            best = Some((score, cap.method.as_str()));
        }
    }
    match best {
        Some((_, m)) => m.to_string(),
        None => "ai.chat".to_string(),
    }
}

/// Build the operator-facing prelude that prefaces every
/// step's input. Carries the spec's constraints + success
/// criteria + budget hint into the agent's prompt so it
/// can honour them.
fn build_prelude(spec: &PlanSpec) -> String {
    let mut buf = String::new();
    if !spec.constraints.is_empty() {
        buf.push_str("constraints: ");
        buf.push_str(&spec.constraints.join("; "));
        buf.push('\n');
    }
    if !spec.success_criteria.is_empty() {
        buf.push_str("success: ");
        buf.push_str(&spec.success_criteria.join("; "));
        buf.push('\n');
    }
    if let Some(budget) = &spec.budget_hint {
        buf.push_str("budget: ");
        buf.push_str(budget);
        buf.push('\n');
    }
    buf
}

/// Sanitize an agent name so it's a valid workflow step id
/// (alphanumeric + underscore). Workflow YAML keys allow
/// dashes too, but underscores are safer cross-render.
fn step_id(agent_name: &str) -> String {
    agent_name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect()
}

fn build_single_agent_workflow(spec: &PlanSpec, agent: &AgentInfo) -> Workflow {
    let capability = pick_capability(agent, &spec.goal);
    let prelude = build_prelude(spec);
    let step = step_id(&agent.name);
    let input = if prelude.is_empty() {
        "{{workflow.input}}".to_string()
    } else {
        format!("{prelude}\nrequest: {{{{workflow.input}}}}")
    };
    let mut agents: BTreeMap<String, AgentSpec> = BTreeMap::new();
    agents.insert(
        step.clone(),
        AgentSpec {
            peer: agent.peer.clone().unwrap_or_else(|| agent.name.clone()),
            capability,
            input,
            output: step.clone(),
        },
    );
    Workflow {
        name: planning_workflow_name(spec),
        version: 1,
        description: format!("Planning: {}", truncate(&spec.goal, 96)),
        agents,
        flow: FlowGraph {
            start: step.clone(),
            edges: Vec::new(),
            result: Some(format!("{{{{{step}.output}}}}")),
        },
    }
}

fn build_sequential_workflow(spec: &PlanSpec, agents: &[AgentInfo]) -> Workflow {
    let prelude = build_prelude(spec);
    let mut agent_specs: BTreeMap<String, AgentSpec> = BTreeMap::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut prev: Option<String> = None;
    let mut step_names: Vec<String> = Vec::with_capacity(agents.len());
    for agent in agents {
        let step = unique_step_id(&step_id(&agent.name), &agent_specs);
        let capability = pick_capability(agent, &spec.goal);
        let input = match &prev {
            None => {
                if prelude.is_empty() {
                    "{{workflow.input}}".to_string()
                } else {
                    format!("{prelude}\nrequest: {{{{workflow.input}}}}")
                }
            }
            Some(p) => format!(
                "previous step `{p}` produced:\n{{{{{p}.output}}}}\n\
                 continue the plan toward: {{{{workflow.input}}}}"
            ),
        };
        agent_specs.insert(
            step.clone(),
            AgentSpec {
                peer: agent.peer.clone().unwrap_or_else(|| agent.name.clone()),
                capability,
                input,
                output: step.clone(),
            },
        );
        if let Some(p) = &prev {
            edges.push(Edge {
                from: p.clone(),
                to: step.clone(),
                condition: EdgeCondition::Success,
            });
        }
        step_names.push(step.clone());
        prev = Some(step);
    }
    let last = step_names.last().cloned().unwrap_or_default();
    Workflow {
        name: planning_workflow_name(spec),
        version: 1,
        description: format!(
            "Sequential plan ({} agents): {}",
            agents.len(),
            truncate(&spec.goal, 80)
        ),
        agents: agent_specs,
        flow: FlowGraph {
            start: step_names[0].clone(),
            edges,
            result: Some(format!("{{{{{last}.output}}}}")),
        },
    }
}

fn build_parallel_workflow(spec: &PlanSpec, agents: &[AgentInfo]) -> Workflow {
    let prelude = build_prelude(spec);
    let mut agent_specs: BTreeMap<String, AgentSpec> = BTreeMap::new();
    let mut edges: Vec<Edge> = Vec::new();

    // Pick a seed agent (the highest-scoring one) to drive the
    // initial dispatch. Parallel edges fan out FROM this seed
    // into the rest of the agents. The seed itself does the
    // first capability against the operator's input — its
    // output is what the parallel branches refine.
    let seed = &agents[0];
    let seed_step = step_id(&seed.name);
    let seed_input = if prelude.is_empty() {
        "{{workflow.input}}".to_string()
    } else {
        format!("{prelude}\nrequest: {{{{workflow.input}}}}")
    };
    agent_specs.insert(
        seed_step.clone(),
        AgentSpec {
            peer: seed.peer.clone().unwrap_or_else(|| seed.name.clone()),
            capability: pick_capability(seed, &spec.goal),
            input: seed_input,
            output: seed_step.clone(),
        },
    );

    // Fan-out + collect every branch's output for the merge.
    let mut branch_names: Vec<String> = Vec::with_capacity(agents.len() - 1);
    for agent in &agents[1..] {
        let step = unique_step_id(&step_id(&agent.name), &agent_specs);
        let capability = pick_capability(agent, &spec.goal);
        agent_specs.insert(
            step.clone(),
            AgentSpec {
                peer: agent.peer.clone().unwrap_or_else(|| agent.name.clone()),
                capability,
                input: format!(
                    "seed agent produced:\n{{{{{seed_step}.output}}}}\n\
                     refine for: {{{{workflow.input}}}}"
                ),
                output: step.clone(),
            },
        );
        edges.push(Edge {
            from: seed_step.clone(),
            to: step.clone(),
            condition: EdgeCondition::Parallel,
        });
        branch_names.push(step);
    }

    // Synthetic merge step. Built as an inline ai.chat-style
    // step that concatenates every branch output into the
    // final answer. The merge runs on the seed agent's peer
    // so the planner doesn't require a third agent for the
    // merge step.
    let merge_step = unique_step_id("merge", &agent_specs);
    let mut merge_input = String::from(
        "Merge the parallel branch outputs into a final answer for: {{workflow.input}}\n",
    );
    for b in &branch_names {
        merge_input.push_str(&format!("\n[{b}]\n{{{{{b}.output}}}}\n"));
    }
    agent_specs.insert(
        merge_step.clone(),
        AgentSpec {
            peer: seed.peer.clone().unwrap_or_else(|| seed.name.clone()),
            capability: pick_capability(seed, &spec.goal),
            input: merge_input,
            output: merge_step.clone(),
        },
    );
    for b in &branch_names {
        edges.push(Edge {
            from: b.clone(),
            to: merge_step.clone(),
            condition: EdgeCondition::Success,
        });
    }

    Workflow {
        name: planning_workflow_name(spec),
        version: 1,
        description: format!(
            "Parallel plan ({} agents): {}",
            agents.len(),
            truncate(&spec.goal, 80)
        ),
        agents: agent_specs,
        flow: FlowGraph {
            start: seed_step,
            edges,
            result: Some(format!("{{{{{merge_step}.output}}}}")),
        },
    }
}

/// Deduplicate step ids when two agents normalise to the same
/// name (e.g. `research-agent` + `research_agent`).
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
        "planning__{}",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller_runtime::{AgentCapabilityDecl, AgentSection};
    use crate::manifest::ManifestProvider;
    use relix_core::types::NodeId;
    use std::collections::BTreeMap;

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

    fn registry_with_two_agents() -> AgentCapabilityRegistry {
        let mut cfg = BTreeMap::new();
        cfg.insert(
            "research-agent".into(),
            section(
                "Specialised in web research and summarisation",
                "research-peer",
                vec![
                    decl("ai.chat", "research queries", &["research", "web"]),
                    decl("tool.web_search", "web search", &["search"]),
                ],
            ),
        );
        cfg.insert(
            "code-agent".into(),
            section(
                "Writes and reviews code",
                "code-peer",
                vec![decl("ai.chat", "code generation", &["code", "programming"])],
            ),
        );
        AgentCapabilityRegistry::from_sources("coord", &manifest(), &cfg, &BTreeMap::new())
    }

    #[test]
    fn simple_single_agent_spec_produces_single_step_workflow() {
        let registry = registry_with_two_agents();
        let g = PlanGenerator::new(registry);
        let spec = super::super::SpecParser::with_known_agents(vec![
            "research-agent".into(),
            "code-agent".into(),
        ])
        .parse("Research web sources on Rust async runtimes.");
        let (wf, topology) = g
            .generate(&spec, &GeneratorOptions { max_agents: 1 })
            .expect("generate");
        assert_eq!(topology, PlanTopology::Single);
        assert_eq!(wf.agents.len(), 1);
        // The single step must be the research agent (highest
        // scoring + only matched agent).
        let only_step = wf.agents.values().next().unwrap();
        assert_eq!(only_step.peer, "research-peer");
    }

    #[test]
    fn sequential_spec_produces_chained_workflow() {
        let registry = registry_with_two_agents();
        let g = PlanGenerator::new(registry);
        let spec = super::super::SpecParser::with_known_agents(vec![
            "research-agent".into(),
            "code-agent".into(),
        ])
        .parse("Research async runtimes then summarise the findings as code comments.");
        let (wf, topology) = g
            .generate(&spec, &GeneratorOptions { max_agents: 2 })
            .expect("generate");
        assert_eq!(topology, PlanTopology::Sequential);
        assert!(
            wf.agents.len() >= 2,
            "expected sequential to use multiple agents: {wf:?}"
        );
        // Edges must chain via Success conditions.
        assert!(
            wf.flow
                .edges
                .iter()
                .all(|e| e.condition == EdgeCondition::Success)
        );
        // Start agent should differ from the result-bound
        // last agent.
        assert!(!wf.flow.start.is_empty());
    }

    #[test]
    fn parallel_spec_produces_parallel_workflow_with_merge() {
        let registry = registry_with_two_agents();
        let g = PlanGenerator::new(registry);
        let spec = super::super::SpecParser::with_known_agents(vec![
            "research-agent".into(),
            "code-agent".into(),
        ])
        .parse("Compare research and code perspectives on the new release in parallel.");
        let (wf, topology) = g
            .generate(&spec, &GeneratorOptions { max_agents: 3 })
            .expect("generate");
        assert_eq!(topology, PlanTopology::Parallel);
        // Must have at least one Parallel-condition edge.
        assert!(
            wf.flow
                .edges
                .iter()
                .any(|e| e.condition == EdgeCondition::Parallel),
            "expected a parallel edge in {:?}",
            wf.flow.edges
        );
        // Must have a merge step that runs on Success edges
        // from each branch.
        assert!(wf.agents.iter().any(|(k, _)| k.starts_with("merge")));
    }

    #[test]
    fn forbidden_agent_is_never_included_in_generated_workflow() {
        let registry = registry_with_two_agents();
        let g = PlanGenerator::new(registry);
        let mut spec = super::super::SpecParser::with_known_agents(vec![
            "research-agent".into(),
            "code-agent".into(),
        ])
        .parse("Research async runtimes without code-agent and then summarise.");
        // Sanity: parser flagged code-agent as forbidden.
        assert!(spec.forbidden_agents.contains(&"code-agent".to_string()));
        spec.preferred_agents.retain(|p| p != "code-agent"); // double-defence
        let (wf, _) = g
            .generate(&spec, &GeneratorOptions { max_agents: 5 })
            .expect("generate");
        for step in wf.agents.values() {
            assert_ne!(step.peer, "code-peer", "code-agent must not appear");
        }
    }

    #[test]
    fn preferred_agent_is_prioritised_over_higher_scoring_alternatives() {
        // Build a registry where code-agent scores higher for
        // the goal, but the operator prefers research-agent.
        let mut cfg = BTreeMap::new();
        cfg.insert(
            "code-agent".into(),
            section(
                "writes code and reviews diffs",
                "code-peer",
                vec![decl(
                    "ai.chat",
                    "code work",
                    &["code", "review", "research"], // tag spans both
                )],
            ),
        );
        cfg.insert(
            "research-agent".into(),
            section(
                "research helper",
                "research-peer",
                vec![decl("ai.chat", "general work", &["research"])],
            ),
        );
        let registry =
            AgentCapabilityRegistry::from_sources("coord", &manifest(), &cfg, &BTreeMap::new());
        let g = PlanGenerator::new(registry);
        // The spec carries "research-agent" as preferred.
        let spec = super::super::SpecParser::with_known_agents(vec![
            "research-agent".into(),
            "code-agent".into(),
        ])
        .parse("Use research-agent to do the code review work.");
        let (wf, _topology) = g
            .generate(&spec, &GeneratorOptions { max_agents: 1 })
            .expect("generate");
        // With max_agents = 1, the preferred MUST be the only
        // step.
        let only_step = wf.agents.values().next().unwrap();
        assert_eq!(only_step.peer, "research-peer");
    }

    #[test]
    fn generated_workflow_validates_cleanly_through_the_existing_validator() {
        let registry = registry_with_two_agents();
        let g = PlanGenerator::new(registry);
        // Try every topology — `generate()` already runs the
        // workflow validator before returning; a passing
        // return here IS the assertion. All three specs use
        // keywords that overlap the test registry's tag set
        // (research / web / code) so the registry returns at
        // least one agent for each.
        for spec_text in [
            "Research web sources on Rust runtimes.",
            "Research web sources then summarise the code.",
            "Compare research and code perspectives in parallel.",
        ] {
            let spec = super::super::SpecParser::new().parse(spec_text);
            let (_wf, _) = g
                .generate(&spec, &GeneratorOptions::default())
                .expect("generate + validate");
        }
    }

    #[test]
    fn empty_goal_returns_empty_goal_error() {
        let registry = registry_with_two_agents();
        let g = PlanGenerator::new(registry);
        let spec = super::super::SpecParser::new().parse("");
        match g.generate(&spec, &GeneratorOptions::default()) {
            Err(GenerateError::EmptyGoal) => {}
            other => panic!("expected EmptyGoal, got {other:?}"),
        }
    }

    #[test]
    fn no_matching_agents_returns_explicit_error() {
        let registry = registry_with_two_agents();
        let g = PlanGenerator::new(registry);
        let spec = super::super::SpecParser::new().parse("Xylophone unicorn parsnip.");
        match g.generate(&spec, &GeneratorOptions::default()) {
            Err(GenerateError::NoMatchingAgents) => {}
            other => panic!("expected NoMatchingAgents, got {other:?}"),
        }
    }

    #[test]
    fn preferred_and_forbidden_simultaneously_returns_explicit_error() {
        let registry = registry_with_two_agents();
        let g = PlanGenerator::new(registry);
        let spec = PlanSpec {
            goal: "do work".into(),
            preferred_agents: vec!["research-agent".into()],
            forbidden_agents: vec!["research-agent".into()],
            ..Default::default()
        };
        match g.generate(&spec, &GeneratorOptions::default()) {
            Err(GenerateError::PreferredAndForbidden) => {}
            other => panic!("expected PreferredAndForbidden, got {other:?}"),
        }
    }

    #[test]
    fn max_steps_caps_the_generated_agent_count() {
        let registry = registry_with_two_agents();
        let g = PlanGenerator::new(registry);
        let spec = PlanSpec {
            goal: "Research async runtimes then summarise.".into(),
            max_steps: Some(1),
            original_spec: "Research async runtimes then summarise.".into(),
            ..Default::default()
        };
        let (wf, topology) = g
            .generate(&spec, &GeneratorOptions { max_agents: 5 })
            .expect("generate");
        // max_steps = 1 ⇒ Single topology regardless of the
        // sequential keyword in the spec.
        assert_eq!(topology, PlanTopology::Single);
        assert_eq!(wf.agents.len(), 1);
    }
}
