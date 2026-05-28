//! RELIX-7.24 — coordinator-side `planning.*` cap handlers.
//!
//! Four unary capabilities, all JSON-encoded:
//!
//! - `planning.list_agents` — every known agent + its
//!   capability summary.
//! - `planning.find_agents` — scored matches for one task
//!   description.
//! - `planning.validate_spec` — parsed [`super::PlanSpec`]
//!   so operators can verify what the parser extracted.
//! - `planning.create_plan` — full pipeline: parse →
//!   generate → optional execute via the existing workflow
//!   engine. Carries `dry_run`.
//!
//! Every handler is a thin wrapper around the planning
//! primitives + (for `create_plan`) the workflow executor.
//! Errors map:
//! - `InvalidArgs` → `error_kinds::INVALID_ARGS` (400 on the bridge)
//! - Engine / workflow failures → `RESPONDER_INTERNAL`

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::{Deserialize, Serialize};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use crate::workflow::{Workflow, WorkflowDispatcher, WorkflowDispatcherCell, execute};

use super::generator::GeneratorOptions;
use super::{AgentCapabilityRegistry, AgentInfo, AgentMatch, PlanGenerator, PlanSpec, SpecParser};

/// Wire every `planning.*` cap onto `bridge`.
pub fn register(
    bridge: &mut DispatchBridge,
    registry: AgentCapabilityRegistry,
    dispatcher_cell: WorkflowDispatcherCell,
) {
    {
        let r = registry.clone();
        bridge.register(
            "planning.list_agents",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let r = r.clone();
                async move { handle_list_agents(&r) }
            })),
        );
    }
    {
        let r = registry.clone();
        bridge.register(
            "planning.find_agents",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let r = r.clone();
                async move { handle_find_agents(&r, &ctx) }
            })),
        );
    }
    {
        let r = registry.clone();
        bridge.register(
            "planning.validate_spec",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let r = r.clone();
                async move { handle_validate_spec(&r, &ctx) }
            })),
        );
    }
    {
        let r = registry;
        let cell = dispatcher_cell;
        bridge.register(
            "planning.create_plan",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let r = r.clone();
                let cell = cell.clone();
                async move { handle_create_plan(&r, &cell, &ctx).await }
            })),
        );
    }
}

/// Static descriptor list mirrors the
/// `*_capability_descriptors()` pattern used by
/// `knowledge::config::sharing_group_descriptors()` etc.
/// Returned by [`super::planning_capability_descriptors`] so
/// the controller-runtime builds manifest entries from one
/// place.
pub fn planning_capability_descriptors() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "planning.list_agents",
            "RELIX-7.24: list every known agent — local synthetic \
             + configured peers + cached remote manifests. \
             Returns `Vec<AgentInfo>` with description, peer alias, \
             and every declared capability (method + description + \
             tags).",
        ),
        (
            "planning.find_agents",
            "RELIX-7.24: score every known agent against a task \
             description. Args JSON: `{task}`. Returns \
             `Vec<AgentMatch>` sorted by descending score; ties \
             broken by name. Score = 3pt per tag match + 2pt per \
             method-name segment match + 1pt per description \
             keyword match.",
        ),
        (
            "planning.validate_spec",
            "RELIX-7.24: parse a natural-language spec into a \
             structured `PlanSpec`. Args JSON: `{spec}`. Returns \
             the parsed PlanSpec carrying goal, constraints, \
             success_criteria, preferred_agents, forbidden_agents, \
             max_steps, budget_hint. Useful for operators to verify \
             the parser understood their intent BEFORE asking the \
             generator to act on it.",
        ),
        (
            "planning.create_plan",
            "RELIX-7.24: full pipeline — parse spec → generate \
             validated workflow → optionally execute via the \
             existing workflow engine. Args JSON: `{spec, \
             max_agents?, dry_run?}`. When `dry_run = true` \
             returns `{plan_spec, topology, workflow_yaml, \
             agents_selected}` without executing. When `dry_run = \
             false` (default) the generated workflow runs through \
             the wired `WorkflowDispatcher` and the result \
             carries `{plan_spec, topology, workflow_yaml, \
             agents_selected, execution}` where `execution` is \
             the `WorkflowResult` envelope.",
        ),
    ]
}

// ── handlers ─────────────────────────────────────────────

fn handle_list_agents(registry: &AgentCapabilityRegistry) -> HandlerOutcome {
    let agents = registry.list_agents();
    ok_json(&ListAgentsResponse { agents })
}

#[derive(Debug, Deserialize, Default)]
struct FindAgentsArgs {
    #[serde(default)]
    task: String,
}

fn handle_find_agents(registry: &AgentCapabilityRegistry, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: FindAgentsArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.task.trim().is_empty() {
        return invalid("task is required");
    }
    let matches = registry.find_agents_for_task(&args.task);
    ok_json(&FindAgentsResponse { matches })
}

#[derive(Debug, Deserialize, Default)]
struct ValidateSpecArgs {
    #[serde(default)]
    spec: String,
}

fn handle_validate_spec(registry: &AgentCapabilityRegistry, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: ValidateSpecArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.spec.trim().is_empty() {
        return invalid("spec is required");
    }
    let known: Vec<String> = registry.list_agents().into_iter().map(|a| a.name).collect();
    let parser = SpecParser::with_known_agents(known);
    let plan_spec = parser.parse(&args.spec);
    ok_json(&plan_spec)
}

#[derive(Debug, Deserialize, Default)]
struct CreatePlanArgs {
    #[serde(default)]
    spec: String,
    #[serde(default)]
    max_agents: Option<usize>,
    #[serde(default)]
    dry_run: bool,
}

async fn handle_create_plan(
    registry: &AgentCapabilityRegistry,
    dispatcher_cell: &WorkflowDispatcherCell,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let args: CreatePlanArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.spec.trim().is_empty() {
        return invalid("spec is required");
    }

    // Parse spec.
    let known: Vec<String> = registry.list_agents().into_iter().map(|a| a.name).collect();
    let parser = SpecParser::with_known_agents(known);
    let plan_spec = parser.parse(&args.spec);

    // Generate workflow.
    let generator = PlanGenerator::new(registry.clone());
    let opts = GeneratorOptions {
        max_agents: args.max_agents.unwrap_or(3).clamp(1, 16),
    };
    let (workflow, topology) = match generator.generate(&plan_spec, &opts) {
        Ok(v) => v,
        Err(super::generator::GenerateError::EmptyGoal) => {
            return invalid("spec has no extractable goal");
        }
        Err(super::generator::GenerateError::PreferredAndForbidden) => {
            return invalid("spec contains an agent in both preferred and forbidden lists");
        }
        Err(super::generator::GenerateError::NoMatchingAgents) => {
            return invalid("no configured agents match the spec goal");
        }
        Err(super::generator::GenerateError::InvalidWorkflow(m)) => {
            return internal_msg(&format!("generated workflow failed validation: {m}"));
        }
    };

    let agents_selected: Vec<AgentInfo> = workflow
        .agents
        .values()
        .filter_map(|spec| {
            // Step `peer` is the operator's peer alias OR the
            // agent's own name; map back to the registry by
            // either.
            registry
                .list_agents()
                .into_iter()
                .find(|a| a.peer.as_deref() == Some(spec.peer.as_str()) || a.name == spec.peer)
        })
        .collect();

    let workflow_yaml = render_workflow_yaml(&workflow);

    let mut response = CreatePlanResponse {
        plan_spec,
        topology: format!("{topology:?}").to_lowercase(),
        workflow_name: workflow.name.clone(),
        workflow_yaml,
        agents_selected,
        execution: None,
    };

    if args.dry_run {
        return ok_json(&response);
    }

    // Execute the workflow via the wired dispatcher.
    let Some(dispatcher) = dispatcher_cell.get().cloned() else {
        return internal_msg(
            "planning.create_plan: no workflow dispatcher wired — cannot execute. \
             Retry with dry_run = true to inspect the generated workflow.",
        );
    };
    let dispatcher: Arc<dyn WorkflowDispatcher> = dispatcher;
    let workflow_arc = Arc::new(workflow);
    let result = execute(workflow_arc.clone(), dispatcher, &response.plan_spec.goal).await;
    response.execution = Some(ExecutionSummary::from_result(&result));
    ok_json(&response)
}

// ── wire types ────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ListAgentsResponse {
    agents: Vec<AgentInfo>,
}

#[derive(Debug, Serialize)]
struct FindAgentsResponse {
    matches: Vec<AgentMatch>,
}

#[derive(Debug, Serialize)]
struct CreatePlanResponse {
    plan_spec: PlanSpec,
    /// `"single"`, `"sequential"`, `"parallel"`.
    topology: String,
    workflow_name: String,
    /// The full YAML representation of the generated
    /// workflow. Operators can feed this directly to
    /// `workflow.run` after editing, or hand it to
    /// `workflow.validate` to confirm structural integrity.
    workflow_yaml: String,
    /// Agent profiles the planner selected. Useful for the
    /// operator-facing CLI to print "selected: research-agent
    /// (research-peer)" without re-querying the registry.
    agents_selected: Vec<AgentInfo>,
    /// Populated only when `dry_run = false` — the result of
    /// running the generated workflow through the existing
    /// executor.
    #[serde(skip_serializing_if = "Option::is_none")]
    execution: Option<ExecutionSummary>,
}

#[derive(Debug, Serialize)]
struct ExecutionSummary {
    execution_id: String,
    status: String,
    result: String,
    total_latency_ms: u64,
}

impl ExecutionSummary {
    fn from_result(result: &crate::workflow::WorkflowResult) -> Self {
        Self {
            execution_id: format!("{}", result.trace.execution_id),
            status: format!("{:?}", result.status).to_lowercase(),
            result: result.result.clone(),
            total_latency_ms: result.trace.total_latency_ms,
        }
    }
}

// ── helpers ────────────────────────────────────────────

/// Render a [`Workflow`] back to YAML. The workflow YAML
/// parser is round-trip-friendly via serde, but the AST
/// types don't derive `Serialize` for ordered key
/// preservation. We emit a deterministic minimal YAML
/// instead — operators get a clean string they can paste
/// into a `.yaml` file and feed back through `workflow.run`.
fn render_workflow_yaml(wf: &Workflow) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "name: {}", yaml_string(&wf.name));
    let _ = writeln!(out, "version: {}", wf.version);
    if !wf.description.is_empty() {
        let _ = writeln!(out, "description: {}", yaml_string(&wf.description));
    }
    let _ = writeln!(out, "agents:");
    for (id, spec) in &wf.agents {
        let _ = writeln!(out, "  {id}:");
        let _ = writeln!(out, "    peer: {}", yaml_string(&spec.peer));
        let _ = writeln!(out, "    capability: {}", yaml_string(&spec.capability));
        let _ = writeln!(out, "    input: {}", yaml_block_scalar(&spec.input));
        let _ = writeln!(out, "    output: {}", yaml_string(&spec.output));
    }
    let _ = writeln!(out, "flow:");
    let _ = writeln!(out, "  start: {}", yaml_string(&wf.flow.start));
    if !wf.flow.edges.is_empty() {
        let _ = writeln!(out, "  edges:");
        for e in &wf.flow.edges {
            let cond = match e.condition {
                crate::workflow::EdgeCondition::Success => "success",
                crate::workflow::EdgeCondition::Failure => "failure",
                crate::workflow::EdgeCondition::Always => "always",
                crate::workflow::EdgeCondition::Parallel => "parallel",
            };
            let _ = writeln!(
                out,
                "    - {{ from: {}, to: {}, condition: {} }}",
                yaml_string(&e.from),
                yaml_string(&e.to),
                cond
            );
        }
    }
    if let Some(r) = &wf.flow.result {
        let _ = writeln!(out, "  result: {}", yaml_string(r));
    }
    out
}

/// Quote a YAML scalar conservatively: if it contains
/// special characters OR starts with a sigil, double-quote
/// + escape. Otherwise emit it bare.
fn yaml_string(s: &str) -> String {
    let needs_quote = s.is_empty()
        || s.chars()
            .any(|c| matches!(c, ':' | '#' | '{' | '}' | '[' | ']' | '\n' | '"' | '\''))
        || s.starts_with(|c: char| {
            matches!(c, '-' | '?' | '!' | '*' | '&' | '|' | '>' | '%' | '@' | '`')
        })
        || s.starts_with(' ')
        || s.ends_with(' ');
    if needs_quote {
        let escaped = s
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

/// Multi-line strings get the block-literal `|` form for
/// readability. Falls back to the regular quoted scalar for
/// single-line content.
fn yaml_block_scalar(s: &str) -> String {
    if !s.contains('\n') {
        return yaml_string(s);
    }
    let mut out = String::from("|\n");
    for line in s.lines() {
        out.push_str("      ");
        out.push_str(line);
        out.push('\n');
    }
    // Trim trailing newline — YAML block scalar implicitly
    // ends at the next less-indented line.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

fn decode<T: serde::de::DeserializeOwned + Default>(
    ctx: &InvocationCtx,
) -> Result<T, HandlerOutcome> {
    if ctx.args.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(&ctx.args).map_err(|e| invalid(&format!("decode args: {e}")))
}

fn ok_json<T: serde::Serialize>(value: &T) -> HandlerOutcome {
    match serde_json::to_vec(value) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("planning: encode response: {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

fn invalid(msg: &str) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause: msg.to_string(),
        retry_hint: 0,
        retry_after: None,
    })
}

fn internal_msg(msg: &str) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause: msg.to_string(),
        retry_hint: 0,
        retry_after: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller_runtime::{AgentCapabilityDecl, AgentSection};
    use crate::manifest::ManifestProvider;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use relix_core::policy::PolicyEngine;
    use relix_core::types::NodeId;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn fixture_registry() -> AgentCapabilityRegistry {
        let m = ManifestProvider::new(
            NodeId::from_pubkey(b"local"),
            "coord",
            "coordinator",
            NodeId::from_pubkey(b"org"),
            vec![],
        );
        let mut cfg = BTreeMap::new();
        cfg.insert(
            "research-agent".into(),
            AgentSection {
                training: None,
                peer: Some("research-peer".into()),
                description: Some("Specialised in web research".into()),
                capabilities: vec![AgentCapabilityDecl {
                    method: "ai.chat".into(),
                    description: Some("research".into()),
                    tags: vec!["research".into(), "web".into()],
                }],
            },
        );
        cfg.insert(
            "code-agent".into(),
            AgentSection {
                training: None,
                peer: Some("code-peer".into()),
                description: Some("Writes and reviews code".into()),
                capabilities: vec![AgentCapabilityDecl {
                    method: "ai.chat".into(),
                    description: Some("code".into()),
                    tags: vec!["code".into()],
                }],
            },
        );
        AgentCapabilityRegistry::from_sources("coord", &m, &cfg, &BTreeMap::new())
    }

    fn fresh_bridge() -> (DispatchBridge, TempDir) {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::permissive();
        let bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        (bridge, dir)
    }

    fn ctx_with(args: &[u8]) -> InvocationCtx {
        use relix_core::identity::VerifiedIdentity;
        use relix_core::types::{NodeId, RequestId, TraceId};
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"caller"),
                name: "alice".into(),
                org_id: NodeId::from_pubkey(b"org"),
                groups: vec!["operators".into()],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: args.to_vec(),
        }
    }

    #[tokio::test]
    async fn caps_register_without_panic() {
        let (mut bridge, _dir) = fresh_bridge();
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        register(&mut bridge, fixture_registry(), cell);
        let _snapshot = bridge.capability_stats_snapshot();
    }

    #[test]
    fn descriptors_cover_every_capability() {
        let methods: Vec<&str> = planning_capability_descriptors()
            .iter()
            .map(|(m, _)| *m)
            .collect();
        for expected in [
            "planning.list_agents",
            "planning.find_agents",
            "planning.validate_spec",
            "planning.create_plan",
        ] {
            assert!(
                methods.contains(&expected),
                "missing descriptor: {expected}"
            );
        }
    }

    #[test]
    fn handle_list_agents_returns_every_known_agent() {
        let r = fixture_registry();
        let HandlerOutcome::Ok(body) = handle_list_agents(&r) else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let agents = v["agents"].as_array().expect("agents array");
        // research-agent + code-agent (no local manifest caps
        // in this fixture, so coordinator is absent).
        let names: Vec<&str> = agents.iter().map(|a| a["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"research-agent"));
        assert!(names.contains(&"code-agent"));
    }

    #[test]
    fn handle_find_agents_rejects_empty_task() {
        let r = fixture_registry();
        let ctx = ctx_with(br#"{"task":""}"#);
        match handle_find_agents(&r, &ctx) {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, relix_core::types::error_kinds::INVALID_ARGS)
            }
            _ => panic!("expected INVALID_ARGS"),
        }
    }

    #[test]
    fn handle_find_agents_returns_scored_matches() {
        let r = fixture_registry();
        let ctx = ctx_with(br#"{"task":"research the web"}"#);
        let HandlerOutcome::Ok(body) = handle_find_agents(&r, &ctx) else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let matches = v["matches"].as_array().unwrap();
        assert!(!matches.is_empty());
        assert_eq!(matches[0]["agent"], "research-agent");
    }

    #[test]
    fn handle_validate_spec_returns_parsed_plan_spec() {
        let r = fixture_registry();
        let ctx =
            ctx_with(br#"{"spec":"Research the web. Use research-agent without code-agent."}"#);
        let HandlerOutcome::Ok(body) = handle_validate_spec(&r, &ctx) else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["goal"], "Research the web");
        assert!(
            v["preferred_agents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a == "research-agent")
        );
        assert!(
            v["forbidden_agents"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a == "code-agent")
        );
    }

    #[tokio::test]
    async fn handle_create_plan_dry_run_returns_workflow_yaml_without_executing() {
        let r = fixture_registry();
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        let ctx = ctx_with(
            br#"{"spec":"Research the web on Rust runtimes.","dry_run":true,"max_agents":1}"#,
        );
        let HandlerOutcome::Ok(body) = handle_create_plan(&r, &cell, &ctx).await else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["topology"], "single");
        assert!(v["workflow_yaml"].as_str().unwrap().contains("agents:"));
        // dry_run = true → no execution summary.
        assert!(v.get("execution").is_none() || v["execution"].is_null());
    }

    #[tokio::test]
    async fn handle_create_plan_rejects_empty_spec() {
        let r = fixture_registry();
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        let ctx = ctx_with(br#"{"spec":""}"#);
        match handle_create_plan(&r, &cell, &ctx).await {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, relix_core::types::error_kinds::INVALID_ARGS)
            }
            _ => panic!("expected INVALID_ARGS"),
        }
    }

    #[tokio::test]
    async fn handle_create_plan_returns_invalid_when_no_agent_matches() {
        let r = fixture_registry();
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        let ctx = ctx_with(br#"{"spec":"xylophone unicorn parsnip","dry_run":true}"#);
        match handle_create_plan(&r, &cell, &ctx).await {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, relix_core::types::error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("no configured agents match"));
            }
            _ => panic!("expected INVALID_ARGS"),
        }
    }

    #[tokio::test]
    async fn handle_create_plan_non_dry_run_without_dispatcher_returns_internal() {
        let r = fixture_registry();
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        // dry_run = false but dispatcher cell is empty.
        let ctx = ctx_with(br#"{"spec":"Research the web on async runtimes.","dry_run":false}"#);
        match handle_create_plan(&r, &cell, &ctx).await {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, relix_core::types::error_kinds::RESPONDER_INTERNAL);
                assert!(env.cause.contains("no workflow dispatcher wired"));
            }
            _ => panic!("expected RESPONDER_INTERNAL"),
        }
    }

    #[test]
    fn render_workflow_yaml_round_trips_through_parser() {
        let r = fixture_registry();
        let g = PlanGenerator::new(r);
        let spec = SpecParser::new().parse("Research the web on Rust runtimes.");
        let (wf, _) = g
            .generate(&spec, &GeneratorOptions { max_agents: 1 })
            .expect("generate");
        let yaml = render_workflow_yaml(&wf);
        let parsed = crate::workflow::parse_str(&yaml)
            .unwrap_or_else(|e| panic!("yaml did not parse: {e}\n---\n{yaml}"));
        crate::workflow::validate(&parsed, None).expect("re-parsed yaml validates");
        assert_eq!(parsed.name, wf.name);
        assert_eq!(parsed.agents.len(), wf.agents.len());
    }

    #[test]
    fn render_workflow_yaml_round_trips_for_sequential_topology() {
        let r = fixture_registry();
        let g = PlanGenerator::new(r);
        let spec = SpecParser::new().parse("Research the web then summarise the code findings.");
        let (wf, topo) = g
            .generate(&spec, &GeneratorOptions::default())
            .expect("generate");
        assert_eq!(topo, super::super::PlanTopology::Sequential);
        let yaml = render_workflow_yaml(&wf);
        let parsed = crate::workflow::parse_str(&yaml)
            .unwrap_or_else(|e| panic!("yaml did not parse: {e}\n---\n{yaml}"));
        crate::workflow::validate(&parsed, None).expect("validates");
    }

    #[test]
    fn render_workflow_yaml_round_trips_for_parallel_topology() {
        let r = fixture_registry();
        let g = PlanGenerator::new(r);
        let spec = SpecParser::new().parse("Compare research and code perspectives in parallel.");
        let (wf, topo) = g
            .generate(&spec, &GeneratorOptions::default())
            .expect("generate");
        assert_eq!(topo, super::super::PlanTopology::Parallel);
        let yaml = render_workflow_yaml(&wf);
        let parsed = crate::workflow::parse_str(&yaml)
            .unwrap_or_else(|e| panic!("yaml did not parse: {e}\n---\n{yaml}"));
        crate::workflow::validate(&parsed, None).expect("validates");
    }
}
