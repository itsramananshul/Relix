//! RELIX-7.24 — coordinator-side `planning.*` cap handlers.
//!
//! Five unary capabilities, all JSON-encoded:
//!
//! - `planning.list_agents` — every known agent + its
//!   capability summary.
//! - `planning.find_agents` — scored matches for one task
//!   description.
//! - `planning.validate_spec` — parsed [`super::PlanSpec`]
//!   so operators can verify what the parser extracted.
//! - `planning.create_plan` — full pipeline: parse → optional
//!   orchestrator → single-agent fallback → conflict resolver
//!   → optional critic loop → optional execute via the
//!   existing workflow engine. Carries `dry_run`.
//! - `planning.orchestrator_status` — read-only view of the
//!   wired [`super::PlanningConfig`] and whether the
//!   orchestrator dispatcher is live. RELIX-7.24 Stage-1/3.
//!
//! Every handler is a thin wrapper around the planning
//! primitives + (for `create_plan`) the workflow executor.
//! Errors map:
//! - `InvalidArgs` → `error_kinds::INVALID_ARGS` (400 on the bridge)
//! - Engine / workflow failures → `RESPONDER_INTERNAL`

use std::sync::Arc;

use async_trait::async_trait;
use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::{Deserialize, Serialize};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use crate::workflow::{Workflow, WorkflowDispatcher, WorkflowDispatcherCell, execute};

use super::critic::{CriticLoop, CriticOutcome, PlanProducer};
use super::generator::GeneratorOptions;
use super::orchestrator::{Orchestrator, OrchestratorOutcome};
use super::{
    AgentCapabilityRegistry, AgentInfo, AgentMatch, ConflictResolutionReport, ConflictResolver,
    PlanGenerator, PlanSpec, PlanningConfig, SpecParser,
};

/// Wire every `planning.*` cap onto `bridge`. The
/// `dispatcher_cell` is the SAME [`WorkflowDispatcherCell`]
/// the workflow engine uses — the orchestrator + critic
/// dispatch their `ai.chat` decomposition + review calls
/// through it. When the cell is empty (mesh not yet wired),
/// the orchestrator's heuristic decomposer and the critic's
/// "implicitly approved with caveat" fallback keep the
/// pipeline running.
///
/// `approval_store` is `Some(...)` when the controller boots
/// with an enabled Stage-4 approval gate; that turns on the
/// `planning.approve_plan` / `.reject_plan` / `.list_approvals`
/// / `.get_approval` capabilities. When `None`, those caps
/// are NOT registered and `planning.create_plan` ignores
/// `require_approval` (silently runs the legacy execute-now
/// path), so existing operators see byte-identical behaviour.
pub fn register(
    bridge: &mut DispatchBridge,
    registry: AgentCapabilityRegistry,
    dispatcher_cell: WorkflowDispatcherCell,
    planning_cfg: PlanningConfig,
    approval_store: Option<super::ApprovalStore>,
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
        let cfg = planning_cfg.clone();
        let cell = dispatcher_cell.clone();
        bridge.register(
            "planning.orchestrator_status",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let cfg = cfg.clone();
                let cell = cell.clone();
                async move { handle_orchestrator_status(&cfg, &cell) }
            })),
        );
    }
    {
        let r = registry.clone();
        let cell = dispatcher_cell.clone();
        let cfg = planning_cfg.clone();
        let store_opt = approval_store.clone();
        bridge.register(
            "planning.create_plan",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let r = r.clone();
                let cell = cell.clone();
                let cfg = cfg.clone();
                let store_opt = store_opt.clone();
                async move { handle_create_plan(&r, &cell, &cfg, store_opt.as_ref(), &ctx).await }
            })),
        );
    }
    if let Some(store) = approval_store {
        {
            let s = store.clone();
            let cell = dispatcher_cell.clone();
            bridge.register(
                "planning.approve_plan",
                Arc::new(FnHandler(move |ctx: InvocationCtx| {
                    let s = s.clone();
                    let cell = cell.clone();
                    async move { handle_approve_plan(&s, &cell, &ctx).await }
                })),
            );
        }
        {
            let s = store.clone();
            bridge.register(
                "planning.reject_plan",
                Arc::new(FnHandler(move |ctx: InvocationCtx| {
                    let s = s.clone();
                    async move { handle_reject_plan(&s, &ctx) }
                })),
            );
        }
        {
            let s = store.clone();
            bridge.register(
                "planning.list_approvals",
                Arc::new(FnHandler(move |ctx: InvocationCtx| {
                    let s = s.clone();
                    async move { handle_list_approvals(&s, &ctx) }
                })),
            );
        }
        {
            let s = store.clone();
            bridge.register(
                "planning.get_approval",
                Arc::new(FnHandler(move |ctx: InvocationCtx| {
                    let s = s.clone();
                    async move { handle_get_approval(&s, &ctx) }
                })),
            );
        }
        {
            let s = store.clone();
            bridge.register(
                "planning.verification_log",
                Arc::new(FnHandler(move |ctx: InvocationCtx| {
                    let s = s.clone();
                    async move { handle_verification_log(&s, &ctx) }
                })),
            );
        }
        {
            let s = store;
            bridge.register(
                "planning.export_spec",
                Arc::new(FnHandler(move |ctx: InvocationCtx| {
                    let s = s.clone();
                    async move { handle_export_spec(&s, &ctx) }
                })),
            );
        }
    }
    // Hold a registry reference so the closures above can
    // outlive the function scope. Without this, the move
    // above for create_plan consumed `registry` and the
    // remaining four caps wouldn't compile.
    let _ = registry;
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
             max_steps, budget_hint, complexity_score, is_complex. \
             Useful for operators to verify the parser understood \
             their intent BEFORE asking the generator to act on it.",
        ),
        (
            "planning.create_plan",
            "RELIX-7.24 (Stage-1 + Stage-3): full pipeline — parse \
             spec → optional orchestrator (decomposes a complex \
             goal into 2-4 sub-goals + assigns specialists + plans \
             in parallel + merges) → conflict resolver (renames \
             duplicate outputs / sequences interfering parallel \
             write calls / drops references to non-existent \
             outputs / escalates unresolvable cases) → optional \
             critic loop (adversarial review against the PlanSpec \
             with up to max_critic_rounds of regenerate-on-reject) \
             → optional execute. Args JSON: `{spec, max_agents?, \
             dry_run?}`. Response always carries `{plan_spec, \
             topology, workflow_name, workflow_yaml, \
             agents_selected, orchestrator_activated, \
             specialist_count, critic_rounds, critic_approved, \
             conflict_resolution_report, execution?}`. When \
             `dry_run = true` the critic loop and execution are \
             both skipped.",
        ),
        (
            "planning.orchestrator_status",
            "RELIX-7.24 Stage-1/3: read-only snapshot of the \
             configured `[planning]` block. Returns \
             `{orchestrator: {enabled, agent, peer, \
             complexity_threshold, max_parallel_specialists}, \
             critic: {enabled, agent, peer, max_rounds}, \
             dispatcher_live}` so operators can confirm the \
             orchestrator + critic are wired and which peer \
             they'll dispatch to.",
        ),
        (
            "planning.approve_plan",
            "RELIX-7.24 Stage-4: approve a pending plan, verify \
             the persisted spec signature has not been \
             tampered with, then execute the stored workflow \
             through the wired WorkflowDispatcher. Args JSON: \
             `{plan_id, note?}`. Returns the updated \
             ApprovalRecord + the WorkflowResult. Errors \
             INVALID_ARGS when the plan_id is unknown, the \
             plan is already decided, or the signature \
             mismatches.",
        ),
        (
            "planning.reject_plan",
            "RELIX-7.24 Stage-4: reject a pending plan. No \
             execution happens. Args JSON: `{plan_id, note?}`. \
             Returns the updated ApprovalRecord. Errors \
             INVALID_ARGS when the plan_id is unknown or the \
             plan is already decided.",
        ),
        (
            "planning.list_approvals",
            "RELIX-7.24 Stage-4: list approval records. Args \
             JSON: `{status?}` — when provided, filters by \
             pending|approved|rejected|expired. Returns \
             `{approvals: [ApprovalRecord]}` newest-first.",
        ),
        (
            "planning.get_approval",
            "RELIX-7.24 Stage-4: fetch one approval record by \
             plan_id. Args JSON: `{plan_id}`. Returns the full \
             ApprovalRecord including spec + workflow_yaml + \
             status + decision metadata.",
        ),
        (
            "planning.verification_log",
            "RELIX-7.24 Stage-5: full step-level verification \
             log for one plan. Args JSON: `{plan_id}`. Returns \
             `{entries: [{step_id, criterion, strategy_used, \
             passed, reason, verified_at_ms}]}` ordered \
             chronologically. Empty array when verification \
             was skipped (verify_steps = false) OR no \
             success_criteria fired for this plan.",
        ),
        (
            "planning.export_spec",
            "RELIX-7.24 follow-up: export a stored plan as a \
             portable artifact for external trackers. Args \
             JSON: `{plan_id, format}` where `format` is one \
             of `\"json\"` (full structured PlanSpec + \
             workflow_yaml + status + decision metadata, with \
             a `schema_version` field set to PLAN_SPEC_VERSION) \
             or `\"markdown\"` (human-readable summary suitable \
             for pasting into Linear / GitHub Issues / Jira). \
             Returns `{plan_id, format, content}`. The spec \
             signature is preserved in the JSON export so \
             consumers can re-verify tamper-evidence.",
        ),
    ]
}

/// RELIX-7.24 Stage-4: spawn a background sweep task that
/// every 60 seconds expires every `pending` approval older
/// than `timeout_secs`. The task runs until the returned
/// [`tokio::task::JoinHandle`] is dropped OR the controller
/// shuts down.
pub fn spawn_approval_expiry_sweep(
    store: super::ApprovalStore,
    timeout_secs: i64,
) -> tokio::task::JoinHandle<()> {
    let timeout_ms = timeout_secs.max(1) * 1000;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        // Skip the first immediate tick — the controller's
        // boot path may still be wiring other components.
        interval.tick().await;
        loop {
            interval.tick().await;
            let now_ms = unix_now_ms();
            let cutoff_ms = now_ms - timeout_ms;
            match store.expire_older_than(cutoff_ms, now_ms) {
                Ok(expired) => {
                    if !expired.is_empty() {
                        tracing::info!(
                            expired_count = expired.len(),
                            cutoff_ms,
                            "planning.approval: expired {} pending plan(s)",
                            expired.len()
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "planning.approval: expiry sweep failed");
                }
            }
        }
    })
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
    /// Per-call override of `[planning] require_approval`.
    /// `None` defers to the global config; `Some(false)`
    /// forces the legacy execute-now path; `Some(true)`
    /// forces the approval gate even when global config has
    /// `require_approval = false`. Useful for the relix-build
    /// CLI which always wants the gate.
    #[serde(default)]
    require_approval: Option<bool>,
}

async fn handle_create_plan(
    registry: &AgentCapabilityRegistry,
    dispatcher_cell: &WorkflowDispatcherCell,
    planning_cfg: &PlanningConfig,
    approval_store: Option<&super::ApprovalStore>,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let args: CreatePlanArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.spec.trim().is_empty() {
        return invalid("spec is required");
    }

    // Parse spec (with complexity score).
    let known: Vec<String> = registry.list_agents().into_iter().map(|a| a.name).collect();
    let parser = SpecParser::with_known_agents(known);
    let plan_spec = parser.parse(&args.spec);

    let opts = GeneratorOptions {
        max_agents: args.max_agents.unwrap_or(3).clamp(1, 16),
    };

    // A "no-op" dispatcher for orchestrator + critic when the
    // mesh cell hasn't been populated yet. Calls always fail
    // → orchestrator falls back to heuristic_decompose, critic
    // exits round 1 with "unreachable" warning. This keeps
    // planning live even before the post-startup dial-and-pin
    // sequence has populated the cell.
    let dispatcher_for_ai = ai_dispatcher(dispatcher_cell);

    // 1. Orchestrator pass.
    let orchestrator = Orchestrator::new(
        registry.clone(),
        dispatcher_for_ai.clone(),
        planning_cfg.orchestrator.clone(),
    );
    let orch_outcome = match orchestrator.orchestrate(&plan_spec, &opts).await {
        Ok(o) => o,
        Err(super::orchestrator::OrchestratorError::EmptyGoal) => {
            return invalid("spec has no extractable goal");
        }
        Err(e) => {
            return internal_msg(&format!("orchestrator failed: {e}"));
        }
    };

    let generator = PlanGenerator::new(registry.clone());

    let (
        mut current_workflow,
        topology_str,
        orchestrator_activated,
        specialist_count,
        sub_goals,
        specialist_assignments,
        decomposed_by_heuristic,
    ) = match orch_outcome {
        OrchestratorOutcome::Active {
            workflow,
            topology,
            sub_goals,
            specialist_assignments,
            decomposed_by_heuristic,
            ..
        } => {
            let count = specialist_assignments.len();
            (
                workflow,
                format!("{topology:?}").to_lowercase(),
                true,
                count,
                sub_goals,
                specialist_assignments,
                decomposed_by_heuristic,
            )
        }
        OrchestratorOutcome::Skipped { .. } => {
            // Single-agent fallback.
            match generator.generate(&plan_spec, &opts) {
                Ok((wf, topo)) => (
                    wf,
                    format!("{topo:?}").to_lowercase(),
                    false,
                    0,
                    Vec::new(),
                    Vec::new(),
                    false,
                ),
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
            }
        }
    };

    // 2. Conflict resolution. Every mutation the resolver
    // performs is also recorded in the PlanSpec's changelog +
    // re-signed via ConflictResolver::record_into_spec so the
    // downstream approval store + verification harness still
    // see a valid signature.
    let resolver = ConflictResolver::new();
    let (resolved_workflow, conflict_report) = resolver.resolve(current_workflow);
    current_workflow = resolved_workflow;
    let mut plan_spec = plan_spec;
    ConflictResolver::record_into_spec(&conflict_report, &mut plan_spec);

    // 3. Critic loop (only on non-dry-run).
    let mut revised_spec_for_response = plan_spec.clone();
    let critic_outcome: CriticOutcome = if args.dry_run {
        CriticLoop::skip(
            current_workflow.clone(),
            plan_spec.clone(),
            "dry_run = true",
        )
    } else {
        let critic = CriticLoop::new(dispatcher_for_ai.clone(), planning_cfg.critic.clone());
        let producer = CoordPlanProducer {
            orchestrator: orchestrator.clone(),
            generator: generator.clone(),
            resolver: resolver.clone(),
            opts: opts.clone(),
        };
        let outcome = critic
            .review(current_workflow.clone(), plan_spec.clone(), &producer)
            .await;
        revised_spec_for_response = outcome.revised_spec.clone();
        current_workflow = outcome.workflow.clone();
        outcome
    };
    let critic_summary = CriticSummary {
        enabled: planning_cfg.critic.critic_enabled,
        rounds: critic_outcome.rounds,
        approved: critic_outcome.approved,
        approved_in_round: critic_outcome.approved_in_round,
        warning: critic_outcome.warning.clone(),
        history: critic_outcome.history.clone(),
    };

    let agents_selected: Vec<AgentInfo> = current_workflow
        .agents
        .values()
        .filter_map(|spec| {
            registry
                .list_agents()
                .into_iter()
                .find(|a| a.peer.as_deref() == Some(spec.peer.as_str()) || a.name == spec.peer)
        })
        .collect();

    let workflow_yaml = render_workflow_yaml(&current_workflow);

    let orchestrator_summary = OrchestratorSummary {
        activated: orchestrator_activated,
        complexity_score: plan_spec.complexity_score,
        complexity_threshold: planning_cfg.orchestrator.complexity_threshold,
        sub_goals,
        specialist_assignments,
        decomposed_by_heuristic,
    };

    let mut response = CreatePlanResponse {
        plan_spec: revised_spec_for_response.clone(),
        topology: topology_str,
        workflow_name: current_workflow.name.clone(),
        workflow_yaml: workflow_yaml.clone(),
        agents_selected,
        execution: None,
        orchestrator_activated,
        specialist_count,
        critic_rounds: critic_summary.rounds,
        critic_approved: critic_summary.approved,
        critic: critic_summary.clone(),
        orchestrator: orchestrator_summary.clone(),
        conflict_resolution_report: if conflict_report.conflicts_detected > 0
            || conflict_report.escalated.is_some()
        {
            Some(conflict_report.clone())
        } else {
            None
        },
        approval: None,
        verification: None,
    };

    // 4. Escalate conflict if unresolved.
    if let Some(reason) = conflict_report.escalated {
        return invalid(&format!(
            "planning.create_plan: conflict could not be resolved — {reason}"
        ));
    }

    if args.dry_run {
        return ok_json(&response);
    }

    // 4b. RELIX-7.24 Stage-4 — gate on approval when
    // configured. Only kicks in when:
    //
    // - the controller booted with an approval_store (caller
    //   supplied Some(...) — None disables the gate entirely
    //   and the legacy execute-now path runs);
    // - require_approval is true (either via the per-call
    //   override or the global [planning] config).
    //
    // When gated: persist the pending plan + fan out
    // notifications + return the pending record to the caller
    // without executing. The operator decides via
    // planning.approve_plan / planning.reject_plan.
    let require_approval = args
        .require_approval
        .unwrap_or(planning_cfg.require_approval);
    if require_approval {
        let Some(store) = approval_store else {
            return internal_msg(
                "planning.create_plan: require_approval = true but no approval store wired — \
                 set [planning] approval_db_path on the coordinator OR omit require_approval.",
            );
        };
        let orchestrator_meta = match serde_json::to_value(&orchestrator_summary) {
            Ok(v) => v,
            Err(e) => {
                return internal_msg(&format!(
                    "planning.create_plan: encode orchestrator metadata: {e}"
                ));
            }
        };
        let critic_meta = match serde_json::to_value(&critic_summary) {
            Ok(v) => v,
            Err(e) => {
                return internal_msg(&format!(
                    "planning.create_plan: encode critic metadata: {e}"
                ));
            }
        };
        let record = super::ApprovalRecord {
            plan_id: revised_spec_for_response.spec_id.clone(),
            spec: revised_spec_for_response.clone(),
            workflow_yaml: workflow_yaml.clone(),
            status: super::ApprovalStatus::Pending,
            created_at_ms: unix_now_ms(),
            decided_at_ms: None,
            decision_note: None,
            orchestrator_meta,
            critic_meta,
        };
        if let Err(e) = store.insert_pending(&record) {
            return internal_msg(&format!(
                "planning.create_plan: persist pending approval: {e}"
            ));
        }
        // Fan out notifications. Best-effort: non-blocking,
        // failures only land in tracing logs so an operator
        // with a broken Telegram peer can still queue plans
        // for approval.
        notify_pending_plan(dispatcher_cell, &planning_cfg.approval_targets, &record).await;
        response.approval = Some(ApprovalSummary {
            plan_id: record.plan_id.clone(),
            status: super::ApprovalStatus::Pending.as_str().to_string(),
            created_at_ms: record.created_at_ms,
            notified_targets: planning_cfg.approval_targets.len(),
        });
        return ok_json(&response);
    }

    // 5. Execute the workflow via the wired dispatcher.
    let Some(dispatcher) = dispatcher_cell.get().cloned() else {
        return internal_msg(
            "planning.create_plan: no workflow dispatcher wired — cannot execute. \
             Retry with dry_run = true to inspect the generated workflow.",
        );
    };
    let dispatcher: Arc<dyn WorkflowDispatcher> = dispatcher;
    // 5a. RELIX-7.24 Stage-5: when verify_steps is enabled
    // AND we have an approval store to persist into, stream
    // step events through the harness so verification rows
    // land in the DB AND the final WorkflowResult status can
    // be overridden when required steps fail.
    if planning_cfg.verification.verify_steps
        && let Some(store) = approval_store
    {
        let harness = super::VerificationHarness::new(
            dispatcher.clone(),
            store.clone(),
            planning_cfg.verification.clone(),
        );
        let workflow_arc = Arc::new(current_workflow);
        let (mut wf_result, outcome) = super::execute_with_verification(
            workflow_arc,
            dispatcher,
            &response.plan_spec.goal,
            harness,
            &response.plan_spec.spec_id,
            &response.plan_spec,
        )
        .await;
        if !outcome.passed {
            // Override the workflow result status so the
            // operator sees the run failed even when the
            // executor's own status was Success — the
            // verification failure IS the failure here.
            wf_result.status = crate::workflow::ExecutionStatus::Failed;
            let failure_count = outcome.critical_failures.len();
            wf_result.result = format!(
                "verification: {failure_count} required-step criterion failure(s); see \
                 planning.verification_log for details. Workflow's own result: {prev}",
                prev = wf_result.result
            );
        }
        response.execution = Some(ExecutionSummary::from_result(&wf_result));
        response.verification = Some(VerificationSummary {
            ran: true,
            passed: outcome.passed,
            total_entries: outcome.entries.len(),
            critical_failures: outcome.critical_failures.len(),
            advisory_failures: outcome.advisory_failures.len(),
            required_steps: planning_cfg.verification.required_steps.clone(),
        });
        return ok_json(&response);
    }
    let workflow_arc = Arc::new(current_workflow);
    let result = execute(workflow_arc.clone(), dispatcher, &response.plan_spec.goal).await;
    response.execution = Some(ExecutionSummary::from_result(&result));
    ok_json(&response)
}

fn handle_orchestrator_status(
    cfg: &PlanningConfig,
    cell: &WorkflowDispatcherCell,
) -> HandlerOutcome {
    let resp = OrchestratorStatusResponse {
        orchestrator: OrchestratorConfigView {
            enabled: cfg.orchestrator.enabled,
            agent: cfg.orchestrator.orchestrator_agent.clone(),
            peer: cfg.orchestrator.orchestrator_peer.clone(),
            complexity_threshold: cfg.orchestrator.complexity_threshold,
            max_parallel_specialists: cfg.orchestrator.max_parallel_specialists,
        },
        critic: CriticConfigView {
            enabled: cfg.critic.critic_enabled,
            agent: cfg.critic.critic_agent.clone(),
            peer: cfg.critic.critic_peer.clone(),
            max_rounds: cfg.critic.max_critic_rounds,
        },
        dispatcher_live: cell.get().is_some(),
    };
    ok_json(&resp)
}

// ── RELIX-7.24 Stage-4 approval handlers ─────────────────

#[derive(Debug, Deserialize, Default)]
struct DecidePlanArgs {
    #[serde(default)]
    plan_id: String,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ListApprovalsArgs {
    /// Optional `"pending"` / `"approved"` / `"rejected"` /
    /// `"expired"` filter. Anything else surfaces as
    /// INVALID_ARGS so operators see typos clearly.
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct GetApprovalArgs {
    #[serde(default)]
    plan_id: String,
}

async fn handle_approve_plan(
    store: &super::ApprovalStore,
    dispatcher_cell: &WorkflowDispatcherCell,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let args: DecidePlanArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.plan_id.trim().is_empty() {
        return invalid("plan_id is required");
    }
    let now_ms = unix_now_ms();
    let record = match store.get(&args.plan_id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return invalid(&format!(
                "planning.approve_plan: plan_id `{}` not found",
                args.plan_id
            ));
        }
        Err(e) => return internal_msg(&format!("approval store: {e}")),
    };
    // Verify the spec signature before we touch the row. A
    // mismatch means the persisted spec has been tampered
    // with since insert_pending; we refuse to execute.
    if let Err(e) = record.spec.verify() {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::INVALID_ARGS,
            cause: format!(
                "planning.approve_plan: spec signature mismatch on plan `{}` — refusing to \
                 execute: {e}",
                args.plan_id
            ),
            retry_hint: 0,
            retry_after: None,
        });
    }
    // Flip status. decide() guards against double-approval
    // via a SQL `WHERE status = 'pending'` clause.
    let updated = match store.decide(
        &args.plan_id,
        super::ApprovalStatus::Approved,
        args.note.as_deref(),
        now_ms,
    ) {
        Ok(r) => r,
        Err(super::ApprovalError::NotFound(_)) => {
            return invalid(&format!(
                "planning.approve_plan: plan_id `{}` not found",
                args.plan_id
            ));
        }
        Err(super::ApprovalError::NotPending { status, .. }) => {
            return invalid(&format!(
                "planning.approve_plan: plan_id `{}` is `{}`, expected `pending`",
                args.plan_id,
                status.as_str()
            ));
        }
        Err(e) => return internal_msg(&format!("approval store: {e}")),
    };
    // Parse the workflow YAML we persisted at submit time
    // and run it through the wired dispatcher. A missing
    // dispatcher cell flips the response to a structured
    // approval-without-execution outcome — operators see the
    // approval landed but execution is deferred until the
    // mesh comes online.
    let workflow = match crate::workflow::parse_str(&updated.workflow_yaml) {
        Ok(w) => w,
        Err(e) => {
            return internal_msg(&format!(
                "planning.approve_plan: stored workflow_yaml failed to re-parse: {e}"
            ));
        }
    };
    let response = ApproveResponse {
        record: updated.clone(),
        execution: None,
    };
    let Some(dispatcher) = dispatcher_cell.get().cloned() else {
        // Approval recorded but mesh not yet up. Operators
        // see the approval went through; execution is
        // deferred. Defensible UX — better than silently
        // failing the whole approve call.
        tracing::warn!(
            plan_id = %updated.plan_id,
            "planning.approve_plan: approval recorded but mesh dispatcher not yet \
             wired — execution deferred"
        );
        return ok_json(&response);
    };
    let workflow_arc = Arc::new(workflow);
    let result = crate::workflow::execute(workflow_arc, dispatcher, &updated.spec.goal).await;
    let response = ApproveResponse {
        record: updated,
        execution: Some(ExecutionSummary::from_result(&result)),
    };
    ok_json(&response)
}

fn handle_reject_plan(store: &super::ApprovalStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: DecidePlanArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.plan_id.trim().is_empty() {
        return invalid("plan_id is required");
    }
    let now_ms = unix_now_ms();
    match store.decide(
        &args.plan_id,
        super::ApprovalStatus::Rejected,
        args.note.as_deref(),
        now_ms,
    ) {
        Ok(r) => ok_json(&r),
        Err(super::ApprovalError::NotFound(_)) => invalid(&format!(
            "planning.reject_plan: plan_id `{}` not found",
            args.plan_id
        )),
        Err(super::ApprovalError::NotPending { status, .. }) => invalid(&format!(
            "planning.reject_plan: plan_id `{}` is `{}`, expected `pending`",
            args.plan_id,
            status.as_str()
        )),
        Err(e) => internal_msg(&format!("approval store: {e}")),
    }
}

fn handle_list_approvals(store: &super::ApprovalStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: ListApprovalsArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    let filter = match args.status.as_deref() {
        None | Some("") => None,
        Some(s) => match super::ApprovalStatus::parse(s) {
            Some(v) => Some(v),
            None => {
                return invalid(&format!(
                    "planning.list_approvals: status must be pending|approved|rejected|expired, \
                     got `{s}`"
                ));
            }
        },
    };
    match store.list(filter) {
        Ok(rows) => ok_json(&ListApprovalsResponse { approvals: rows }),
        Err(e) => internal_msg(&format!("approval store: {e}")),
    }
}

fn handle_get_approval(store: &super::ApprovalStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: GetApprovalArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.plan_id.trim().is_empty() {
        return invalid("plan_id is required");
    }
    match store.get(&args.plan_id) {
        Ok(Some(r)) => ok_json(&r),
        Ok(None) => invalid(&format!(
            "planning.get_approval: plan_id `{}` not found",
            args.plan_id
        )),
        Err(e) => internal_msg(&format!("approval store: {e}")),
    }
}

#[derive(Debug, Deserialize, Default)]
struct VerificationLogArgs {
    #[serde(default)]
    plan_id: String,
}

fn handle_verification_log(store: &super::ApprovalStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: VerificationLogArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.plan_id.trim().is_empty() {
        return invalid("plan_id is required");
    }
    match store.list_verifications(&args.plan_id) {
        Ok(entries) => ok_json(&VerificationLogResponse { entries }),
        Err(e) => internal_msg(&format!("verification log: {e}")),
    }
}

#[derive(Debug, Serialize)]
struct VerificationLogResponse {
    entries: Vec<super::VerificationEntry>,
}

#[derive(Debug, Deserialize, Default)]
struct ExportSpecArgs {
    #[serde(default)]
    plan_id: String,
    #[serde(default = "default_export_format")]
    format: String,
}

fn default_export_format() -> String {
    "json".to_string()
}

#[derive(Debug, Serialize)]
struct ExportResponse {
    plan_id: String,
    format: String,
    content: String,
}

fn handle_export_spec(store: &super::ApprovalStore, ctx: &InvocationCtx) -> HandlerOutcome {
    let args: ExportSpecArgs = match decode(ctx) {
        Ok(a) => a,
        Err(out) => return out,
    };
    if args.plan_id.trim().is_empty() {
        return invalid("plan_id is required");
    }
    let record = match store.get(&args.plan_id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return invalid(&format!(
                "planning.export_spec: plan_id `{}` not found",
                args.plan_id
            ));
        }
        Err(e) => return internal_msg(&format!("approval store: {e}")),
    };
    let format = args.format.trim().to_ascii_lowercase();
    let content = match format.as_str() {
        "json" => match render_export_json(&record) {
            Ok(s) => s,
            Err(e) => {
                return internal_msg(&format!("planning.export_spec: encode JSON: {e}"));
            }
        },
        "markdown" | "md" => render_export_markdown(&record),
        other => {
            return invalid(&format!(
                "planning.export_spec: format must be 'json' or 'markdown', got `{other}`"
            ));
        }
    };
    ok_json(&ExportResponse {
        plan_id: args.plan_id,
        format: if format == "md" {
            "markdown".into()
        } else {
            format
        },
        content,
    })
}

/// JSON export. Wraps the hardened PlanSpec + workflow_yaml +
/// approval/verification metadata in a stable schema with an
/// explicit `schema_version` field so external tools can lock
/// against a known shape.
fn render_export_json(record: &super::ApprovalRecord) -> serde_json::Result<String> {
    let payload = serde_json::json!({
        "schema_version": super::PLAN_SPEC_VERSION,
        "plan_id": record.plan_id,
        "status": record.status.as_str(),
        "created_at_ms": record.created_at_ms,
        "decided_at_ms": record.decided_at_ms,
        "decision_note": record.decision_note,
        "spec": record.spec,
        "workflow_yaml": record.workflow_yaml,
        "orchestrator_meta": record.orchestrator_meta,
        "critic_meta": record.critic_meta,
    });
    serde_json::to_string_pretty(&payload)
}

/// Markdown export. Operator-friendly summary suitable for
/// pasting into a tracker issue. Captures every field that
/// would inform a human reviewer; the structured JSON export
/// is the authoritative artifact for tooling.
fn render_export_markdown(record: &super::ApprovalRecord) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "# Relix Plan {}", record.plan_id);
    let _ = writeln!(out);
    let _ = writeln!(out, "- **Status:** {}", record.status.as_str());
    let _ = writeln!(out, "- **Created:** {} (unix ms)", record.created_at_ms);
    if let Some(d) = record.decided_at_ms {
        let _ = writeln!(out, "- **Decided:** {} (unix ms)", d);
    }
    if let Some(note) = &record.decision_note {
        let _ = writeln!(out, "- **Decision note:** {note}");
    }
    if let Some(sig) = &record.spec.signature {
        let _ = writeln!(out, "- **Signature (blake3):** `{sig}`");
    }
    let _ = writeln!(out, "- **Spec version:** {}", record.spec.version);
    let _ = writeln!(
        out,
        "- **Complexity:** {:.2} (is_complex={})",
        record.spec.complexity_score, record.spec.is_complex
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "## Goal");
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", record.spec.goal);
    let _ = writeln!(out);
    if !record.spec.constraints.is_empty() {
        let _ = writeln!(out, "## Constraints");
        let _ = writeln!(out);
        for c in &record.spec.constraints {
            let _ = writeln!(out, "- {c}");
        }
        let _ = writeln!(out);
    }
    if !record.spec.success_criteria.is_empty() {
        let _ = writeln!(out, "## Success criteria");
        let _ = writeln!(out);
        for s in &record.spec.success_criteria {
            let _ = writeln!(out, "- {s}");
        }
        let _ = writeln!(out);
    }
    if !record.spec.preferred_agents.is_empty() {
        let _ = writeln!(
            out,
            "- **Preferred agents:** {}",
            record.spec.preferred_agents.join(", ")
        );
    }
    if !record.spec.forbidden_agents.is_empty() {
        let _ = writeln!(
            out,
            "- **Forbidden agents:** {}",
            record.spec.forbidden_agents.join(", ")
        );
    }
    if !record.spec.changelog.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "## Changelog");
        let _ = writeln!(out);
        for ch in &record.spec.changelog {
            let _ = writeln!(
                out,
                "- `{}` @ {}ms — {}",
                ch.change_type, ch.changed_at_ms, ch.description
            );
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Generated workflow");
    let _ = writeln!(out);
    let _ = writeln!(out, "```yaml");
    let _ = writeln!(out, "{}", record.workflow_yaml);
    let _ = writeln!(out, "```");
    out
}

#[derive(Debug, Serialize)]
struct ListApprovalsResponse {
    approvals: Vec<super::ApprovalRecord>,
}

#[derive(Debug, Serialize)]
struct ApproveResponse {
    record: super::ApprovalRecord,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution: Option<ExecutionSummary>,
}

/// Best-effort fan-out of the pending-plan notification to
/// every configured [`ApprovalTarget`]. Each target gets ONE
/// `<channel>.send` call via the wired mesh dispatcher. Per-
/// target failures land in tracing logs only — they do NOT
/// fail the approval submission.
async fn notify_pending_plan(
    dispatcher_cell: &WorkflowDispatcherCell,
    targets: &[super::ApprovalTarget],
    record: &super::ApprovalRecord,
) {
    if targets.is_empty() {
        tracing::info!(
            plan_id = %record.plan_id,
            "planning.approval: pending plan recorded but no [planning] approval_targets \
             configured — operator must read planning.list_approvals"
        );
        return;
    }
    let Some(dispatcher) = dispatcher_cell.get().cloned() else {
        tracing::warn!(
            plan_id = %record.plan_id,
            "planning.approval: pending plan recorded but mesh dispatcher not yet wired — \
             notification fan-out skipped"
        );
        return;
    };
    let body = super::format_pending_notification(record, None);
    for target in targets {
        let dispatcher = dispatcher.clone();
        let target = target.clone();
        let body = body.clone();
        let plan_id = record.plan_id.clone();
        tokio::spawn(async move {
            let (method, args_bytes) = match encode_approval_target(&target, &body) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        plan_id = %plan_id,
                        channel = %target.channel,
                        peer = %target.peer,
                        error = %e,
                        "planning.approval: notification encode failed"
                    );
                    return;
                }
            };
            if let Err(e) = dispatcher.dispatch(&target.peer, method, &args_bytes).await {
                tracing::warn!(
                    plan_id = %plan_id,
                    channel = %target.channel,
                    peer = %target.peer,
                    error = %e,
                    "planning.approval: notification dispatch failed"
                );
            }
        });
    }
}

fn encode_approval_target(
    target: &super::ApprovalTarget,
    body: &str,
) -> Result<(&'static str, Vec<u8>), String> {
    let channel = target.channel.trim().to_ascii_lowercase();
    match channel.as_str() {
        "email" => {
            let to = target
                .to
                .as_deref()
                .ok_or_else(|| "email target missing `to` field".to_string())?;
            let subject = target
                .subject
                .clone()
                .unwrap_or_else(|| "Relix planning — approval needed".to_string());
            let args = serde_json::json!({
                "to": [to],
                "subject": subject,
                "body": body,
            });
            let bytes = serde_json::to_vec(&args).map_err(|e| format!("encode: {e}"))?;
            Ok(("email.send", bytes))
        }
        "telegram" => {
            let chat_id = target
                .chat_id
                .as_deref()
                .ok_or_else(|| "telegram target missing `chat_id` field".to_string())?;
            let args = serde_json::json!({
                "chat_id": chat_id,
                "text": body,
            });
            let bytes = serde_json::to_vec(&args).map_err(|e| format!("encode: {e}"))?;
            Ok(("telegram.send", bytes))
        }
        "discord" => {
            let channel_id = target
                .channel_id
                .as_deref()
                .ok_or_else(|| "discord target missing `channel_id` field".to_string())?;
            let args = serde_json::json!({
                "channel_id": channel_id,
                "content": body,
            });
            let bytes = serde_json::to_vec(&args).map_err(|e| format!("encode: {e}"))?;
            Ok(("discord.send", bytes))
        }
        "slack" => {
            let slack_channel = target
                .slack_channel
                .as_deref()
                .ok_or_else(|| "slack target missing `slack_channel` field".to_string())?;
            let args = serde_json::json!({
                "channel": slack_channel,
                "text": body,
            });
            let bytes = serde_json::to_vec(&args).map_err(|e| format!("encode: {e}"))?;
            Ok(("slack.send", bytes))
        }
        other => Err(format!(
            "unknown approval target channel `{other}` (allowed: email / telegram / discord / slack)"
        )),
    }
}

fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// [`PlanProducer`] impl that re-runs the orchestrator-with-
/// fallback + conflict resolver path. Used by the critic loop
/// when a rejected verdict forces revision.
struct CoordPlanProducer {
    orchestrator: Orchestrator,
    generator: PlanGenerator,
    resolver: ConflictResolver,
    opts: GeneratorOptions,
}

#[async_trait]
impl PlanProducer for CoordPlanProducer {
    async fn produce(&self, spec: &PlanSpec) -> Result<Workflow, String> {
        let wf = match self.orchestrator.orchestrate(spec, &self.opts).await {
            Ok(OrchestratorOutcome::Active { workflow, .. }) => workflow,
            Ok(OrchestratorOutcome::Skipped { .. }) => self
                .generator
                .generate(spec, &self.opts)
                .map(|(wf, _)| wf)
                .map_err(|e| e.to_string())?,
            Err(e) => return Err(e.to_string()),
        };
        let (resolved, report) = self.resolver.resolve(wf);
        if let Some(reason) = report.escalated {
            return Err(format!("conflict resolution escalated: {reason}"));
        }
        Ok(resolved)
    }
}

/// Build the dispatcher the orchestrator + critic use to
/// invoke `ai.chat` on the configured planning peers. When
/// the mesh `WorkflowDispatcherCell` is empty the dispatcher
/// returned here always fails — orchestrator + critic both
/// have built-in fallbacks for that case.
fn ai_dispatcher(cell: &WorkflowDispatcherCell) -> Arc<dyn WorkflowDispatcher> {
    if let Some(real) = cell.get().cloned() {
        real
    } else {
        Arc::new(NullAiDispatcher)
    }
}

struct NullAiDispatcher;

#[async_trait]
impl WorkflowDispatcher for NullAiDispatcher {
    async fn dispatch(
        &self,
        peer: &str,
        capability: &str,
        _input: &[u8],
    ) -> crate::workflow::DispatchResult {
        Err(crate::workflow::DispatchError {
            peer: peer.to_string(),
            method: capability.to_string(),
            cause: "planning: mesh dispatcher not yet wired".to_string(),
        })
    }
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
    /// RELIX-7.24 Stage-1: `true` when the orchestrator
    /// decomposed the goal into specialist sub-plans. `false`
    /// when the single-agent path ran (max_agents = 1, low
    /// complexity, orchestrator disabled).
    orchestrator_activated: bool,
    /// Number of specialists assigned by the orchestrator.
    /// `0` when the orchestrator was skipped.
    specialist_count: usize,
    /// Number of critic review rounds that ran. `0` when
    /// the critic was skipped (dry_run, disabled).
    critic_rounds: usize,
    /// `true` when the critic approved the final plan.
    /// `true` also when the critic was skipped (dry_run /
    /// disabled) — the absent-critic state is conveyed
    /// through `critic.rounds == 0` + `critic.warning`.
    critic_approved: bool,
    /// Full orchestrator metadata.
    orchestrator: OrchestratorSummary,
    /// Full critic metadata (review history, warning,
    /// approved_in_round).
    critic: CriticSummary,
    /// Present only when at least one conflict was
    /// detected during conflict resolution.
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict_resolution_report: Option<ConflictResolutionReport>,
    /// RELIX-7.24 Stage-4 — present only when the call hit
    /// the approval gate and the plan was persisted into the
    /// approval store in `pending` state. `None` for dry-run
    /// requests AND for calls that bypassed the gate (the
    /// legacy execute-now path).
    #[serde(skip_serializing_if = "Option::is_none")]
    approval: Option<ApprovalSummary>,
    /// RELIX-7.24 Stage-5 — present only when `verify_steps`
    /// was enabled AND the run actually executed (not on
    /// dry-runs or approval-gated submissions).
    #[serde(skip_serializing_if = "Option::is_none")]
    verification: Option<VerificationSummary>,
}

/// RELIX-7.24 Stage-5 — operator-facing summary of the
/// step-level verification harness's verdict.
#[derive(Clone, Debug, Serialize)]
struct VerificationSummary {
    ran: bool,
    passed: bool,
    total_entries: usize,
    critical_failures: usize,
    advisory_failures: usize,
    required_steps: Vec<String>,
}

/// Orchestrator-specific block of the response.
#[derive(Clone, Debug, Serialize)]
struct OrchestratorSummary {
    activated: bool,
    complexity_score: f32,
    complexity_threshold: f32,
    sub_goals: Vec<String>,
    specialist_assignments: Vec<super::orchestrator::SpecialistAssignment>,
    decomposed_by_heuristic: bool,
}

/// Critic-specific block of the response.
#[derive(Clone, Debug, Serialize)]
struct CriticSummary {
    enabled: bool,
    rounds: usize,
    approved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    approved_in_round: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
    history: Vec<super::critic::CriticVerdict>,
}

/// RELIX-7.24 Stage-4 — operator-facing summary of the
/// pending-approval state when `require_approval` is on.
#[derive(Clone, Debug, Serialize)]
struct ApprovalSummary {
    plan_id: String,
    /// `"pending"` on the create_plan response (Stage-4
    /// gating only ever leaves the plan pending). The full
    /// status field is on the [`approve / reject / list /
    /// get`] responses.
    status: String,
    created_at_ms: i64,
    /// Number of channels we attempted to fan out the
    /// pending-plan notification to. `0` when no
    /// `[planning] approval_targets` are configured.
    notified_targets: usize,
}

#[derive(Debug, Serialize)]
struct OrchestratorStatusResponse {
    orchestrator: OrchestratorConfigView,
    critic: CriticConfigView,
    /// `true` when the workflow dispatcher cell has been
    /// populated; orchestrator + critic AI calls will land on
    /// the mesh. `false` while the controller is still
    /// booting OR when no peers are configured — both reach
    /// the heuristic fallback.
    dispatcher_live: bool,
}

#[derive(Debug, Serialize)]
struct OrchestratorConfigView {
    enabled: bool,
    agent: String,
    peer: String,
    complexity_threshold: f32,
    max_parallel_specialists: usize,
}

#[derive(Debug, Serialize)]
struct CriticConfigView {
    enabled: bool,
    agent: String,
    peer: String,
    max_rounds: usize,
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
            tenant_id: None,
        }
    }

    #[tokio::test]
    async fn caps_register_without_panic() {
        let (mut bridge, _dir) = fresh_bridge();
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        register(
            &mut bridge,
            fixture_registry(),
            cell,
            PlanningConfig::default(),
            None,
        );
        let _snapshot = bridge.capability_stats_snapshot();
    }

    #[tokio::test]
    async fn caps_register_with_approval_store_wires_all_nine_caps() {
        let (mut bridge, _dir) = fresh_bridge();
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        let store = super::super::ApprovalStore::open_in_memory().unwrap();
        register(
            &mut bridge,
            fixture_registry(),
            cell,
            PlanningConfig::default(),
            Some(store),
        );
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
            "planning.orchestrator_status",
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
        let cfg = PlanningConfig::default();
        let ctx = ctx_with(
            br#"{"spec":"Research the web on Rust runtimes.","dry_run":true,"max_agents":1}"#,
        );
        let HandlerOutcome::Ok(body) = handle_create_plan(&r, &cell, &cfg, None, &ctx).await else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["topology"], "single");
        assert!(v["workflow_yaml"].as_str().unwrap().contains("agents:"));
        // dry_run = true → no execution summary.
        assert!(v.get("execution").is_none() || v["execution"].is_null());
        // max_agents = 1 → orchestrator skipped.
        assert_eq!(v["orchestrator_activated"], false);
        assert_eq!(v["specialist_count"], 0);
        // dry_run skips critic — 0 rounds, approved (skipped).
        assert_eq!(v["critic_rounds"], 0);
        assert_eq!(v["critic_approved"], true);
    }

    #[tokio::test]
    async fn handle_create_plan_orchestrator_activates_for_complex_spec_under_dry_run() {
        let r = fixture_registry();
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        let cfg = PlanningConfig::default();
        // Complex spec: long goal + two output types →
        // complexity_score >= 0.6, max_agents > 1 → orchestrator
        // activates. Dispatcher cell is empty so the
        // orchestrator falls back to heuristic_decompose.
        let body = serde_json::json!({
            "spec": "Research the web and produce a report and also write code to summarise. \
                    Return a markdown report under 300 words. Produce findings as code comments.",
            "dry_run": true,
            "max_agents": 3,
        });
        let ctx = ctx_with(serde_json::to_vec(&body).unwrap().as_slice());
        let HandlerOutcome::Ok(out) = handle_create_plan(&r, &cell, &cfg, None, &ctx).await else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["orchestrator_activated"], true, "v={v}");
        assert!(v["specialist_count"].as_u64().unwrap() >= 1);
        // The plan_spec carries the complexity score the
        // parser computed.
        let score = v["plan_spec"]["complexity_score"].as_f64().unwrap();
        assert!(score >= 0.6, "complexity_score = {score}");
    }

    #[tokio::test]
    async fn handle_create_plan_rejects_empty_spec() {
        let r = fixture_registry();
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        let cfg = PlanningConfig::default();
        let ctx = ctx_with(br#"{"spec":""}"#);
        match handle_create_plan(&r, &cell, &cfg, None, &ctx).await {
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
        let cfg = PlanningConfig::default();
        let ctx = ctx_with(br#"{"spec":"xylophone unicorn parsnip","dry_run":true}"#);
        match handle_create_plan(&r, &cell, &cfg, None, &ctx).await {
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
        let cfg = PlanningConfig::default();
        // dry_run = false but dispatcher cell is empty.
        let ctx = ctx_with(br#"{"spec":"Research the web on async runtimes.","dry_run":false}"#);
        match handle_create_plan(&r, &cell, &cfg, None, &ctx).await {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, relix_core::types::error_kinds::RESPONDER_INTERNAL);
                assert!(env.cause.contains("no workflow dispatcher wired"));
            }
            _ => panic!("expected RESPONDER_INTERNAL"),
        }
    }

    #[test]
    fn handle_orchestrator_status_reports_configured_values() {
        let cfg = PlanningConfig::default();
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        let HandlerOutcome::Ok(body) = handle_orchestrator_status(&cfg, &cell) else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["orchestrator"]["enabled"], true);
        assert_eq!(v["orchestrator"]["agent"], "coordinator");
        assert_eq!(v["critic"]["enabled"], true);
        assert_eq!(v["critic"]["max_rounds"], 3);
        // Empty cell → dispatcher_live = false.
        assert_eq!(v["dispatcher_live"], false);
    }

    // ── RELIX-7.24 Stage-4 approval handlers ──────────

    #[tokio::test]
    async fn handle_create_plan_with_require_approval_persists_pending_and_returns_plan_id() {
        let r = fixture_registry();
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        let cfg = PlanningConfig {
            require_approval: true,
            ..PlanningConfig::default()
        };
        let store = super::super::ApprovalStore::open_in_memory().unwrap();
        let ctx = ctx_with(
            br#"{"spec":"Research the web on Rust runtimes.","dry_run":false,"max_agents":1}"#,
        );
        let HandlerOutcome::Ok(body) =
            handle_create_plan(&r, &cell, &cfg, Some(&store), &ctx).await
        else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let plan_id = v["approval"]["plan_id"].as_str().expect("plan_id");
        assert_eq!(v["approval"]["status"], "pending");
        // The record landed in the store.
        let stored = store.get(plan_id).unwrap().expect("stored");
        assert_eq!(stored.status, super::super::ApprovalStatus::Pending);
        // Spec signature still verifies.
        stored.spec.verify().expect("signature");
    }

    #[tokio::test]
    async fn handle_create_plan_with_require_approval_but_no_store_returns_internal() {
        let r = fixture_registry();
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        let cfg = PlanningConfig {
            require_approval: true,
            ..PlanningConfig::default()
        };
        let ctx = ctx_with(br#"{"spec":"Research the web.","max_agents":1}"#);
        match handle_create_plan(&r, &cell, &cfg, None, &ctx).await {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, relix_core::types::error_kinds::RESPONDER_INTERNAL);
                assert!(env.cause.contains("no approval store wired"));
            }
            _ => panic!("expected RESPONDER_INTERNAL"),
        }
    }

    #[tokio::test]
    async fn handle_create_plan_per_call_override_forces_approval() {
        let r = fixture_registry();
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        // Global cfg has require_approval = false but the
        // request asks for approval explicitly.
        let cfg = PlanningConfig::default();
        let store = super::super::ApprovalStore::open_in_memory().unwrap();
        let ctx =
            ctx_with(br#"{"spec":"Research the web.","max_agents":1,"require_approval":true}"#);
        let HandlerOutcome::Ok(body) =
            handle_create_plan(&r, &cell, &cfg, Some(&store), &ctx).await
        else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["approval"]["status"], "pending");
    }

    fn seed_pending_record(store: &super::super::ApprovalStore, plan_id: &str) {
        let mut spec = SpecParser::new().parse("Research the web.");
        spec.spec_id = plan_id.to_string();
        let _ = spec.sign();
        let record = super::super::ApprovalRecord {
            plan_id: plan_id.to_string(),
            spec,
            workflow_yaml: "name: x\nversion: 1\nagents:\n  a:\n    peer: p1\n    capability: ai.chat\n    input: hi\n    output: a\nflow:\n  start: a\n".into(),
            status: super::super::ApprovalStatus::Pending,
            created_at_ms: 100,
            decided_at_ms: None,
            decision_note: None,
            orchestrator_meta: serde_json::Value::Null,
            critic_meta: serde_json::Value::Null,
        };
        store.insert_pending(&record).unwrap();
    }

    #[tokio::test]
    async fn handle_approve_plan_without_dispatcher_records_approval_without_executing() {
        let store = super::super::ApprovalStore::open_in_memory().unwrap();
        seed_pending_record(&store, "approve-1");
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        let ctx = ctx_with(br#"{"plan_id":"approve-1","note":"looks good"}"#);
        let HandlerOutcome::Ok(body) = handle_approve_plan(&store, &cell, &ctx).await else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["record"]["status"], "approved");
        assert_eq!(v["record"]["decision_note"], "looks good");
        // Execution is None because no dispatcher was wired.
        assert!(v.get("execution").is_none() || v["execution"].is_null());
        // Persisted state confirms approval.
        let r = store.get("approve-1").unwrap().unwrap();
        assert_eq!(r.status, super::super::ApprovalStatus::Approved);
    }

    #[tokio::test]
    async fn handle_approve_plan_rejects_tampered_spec() {
        let store = super::super::ApprovalStore::open_in_memory().unwrap();
        // Insert a record whose signature does not match the
        // stored spec — simulating tampering at rest.
        let mut spec = SpecParser::new().parse("Research the web.");
        spec.spec_id = "tampered".into();
        // Sign first.
        let _ = spec.sign();
        // Then mutate AFTER signing — verify() will now fail.
        spec.goal = "Hack the planet.".into();
        let record = super::super::ApprovalRecord {
            plan_id: "tampered".into(),
            spec,
            workflow_yaml: "name: x\nversion: 1\nagents:\n  a:\n    peer: p\n    capability: ai.chat\n    input: a\n    output: a\nflow:\n  start: a\n".into(),
            status: super::super::ApprovalStatus::Pending,
            created_at_ms: 1,
            decided_at_ms: None,
            decision_note: None,
            orchestrator_meta: serde_json::Value::Null,
            critic_meta: serde_json::Value::Null,
        };
        store.insert_pending(&record).unwrap();
        let cell: WorkflowDispatcherCell = Arc::new(tokio::sync::OnceCell::new());
        let ctx = ctx_with(br#"{"plan_id":"tampered"}"#);
        match handle_approve_plan(&store, &cell, &ctx).await {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, relix_core::types::error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("signature mismatch"), "{}", env.cause);
            }
            _ => panic!("expected INVALID_ARGS"),
        }
        // Record stays pending — refusal is non-destructive.
        let r = store.get("tampered").unwrap().unwrap();
        assert_eq!(r.status, super::super::ApprovalStatus::Pending);
    }

    #[tokio::test]
    async fn handle_reject_plan_marks_record_rejected_without_executing() {
        let store = super::super::ApprovalStore::open_in_memory().unwrap();
        seed_pending_record(&store, "rej-1");
        let ctx = ctx_with(br#"{"plan_id":"rej-1","note":"out of scope"}"#);
        let HandlerOutcome::Ok(body) = handle_reject_plan(&store, &ctx) else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "rejected");
        assert_eq!(v["decision_note"], "out of scope");
        let r = store.get("rej-1").unwrap().unwrap();
        assert_eq!(r.status, super::super::ApprovalStatus::Rejected);
    }

    #[test]
    fn handle_list_approvals_filters_by_status() {
        let store = super::super::ApprovalStore::open_in_memory().unwrap();
        seed_pending_record(&store, "p1");
        seed_pending_record(&store, "p2");
        // Decide p2.
        store
            .decide(
                "p2",
                super::super::ApprovalStatus::Approved,
                Some("ok"),
                999,
            )
            .unwrap();
        let ctx = ctx_with(br#"{"status":"pending"}"#);
        let HandlerOutcome::Ok(body) = handle_list_approvals(&store, &ctx) else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let approvals = v["approvals"].as_array().unwrap();
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0]["plan_id"], "p1");
    }

    #[test]
    fn handle_list_approvals_rejects_invalid_status() {
        let store = super::super::ApprovalStore::open_in_memory().unwrap();
        let ctx = ctx_with(br#"{"status":"definitely-not-a-status"}"#);
        match handle_list_approvals(&store, &ctx) {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, relix_core::types::error_kinds::INVALID_ARGS);
            }
            _ => panic!("expected INVALID_ARGS"),
        }
    }

    #[test]
    fn handle_get_approval_returns_record_or_not_found() {
        let store = super::super::ApprovalStore::open_in_memory().unwrap();
        seed_pending_record(&store, "g1");
        // Hit.
        let ctx = ctx_with(br#"{"plan_id":"g1"}"#);
        let HandlerOutcome::Ok(body) = handle_get_approval(&store, &ctx) else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["plan_id"], "g1");
        // Miss.
        let ctx = ctx_with(br#"{"plan_id":"ghost"}"#);
        match handle_get_approval(&store, &ctx) {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, relix_core::types::error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("not found"));
            }
            _ => panic!("expected INVALID_ARGS"),
        }
    }

    #[test]
    fn approval_descriptors_present() {
        let methods: Vec<&str> = planning_capability_descriptors()
            .iter()
            .map(|(m, _)| *m)
            .collect();
        for expected in [
            "planning.approve_plan",
            "planning.reject_plan",
            "planning.list_approvals",
            "planning.get_approval",
            "planning.verification_log",
            "planning.export_spec",
        ] {
            assert!(
                methods.contains(&expected),
                "missing descriptor: {expected}"
            );
        }
    }

    #[test]
    fn handle_export_spec_json_round_trips_signature() {
        let store = super::super::ApprovalStore::open_in_memory().unwrap();
        seed_pending_record(&store, "exp-json");
        let ctx = ctx_with(br#"{"plan_id":"exp-json","format":"json"}"#);
        let HandlerOutcome::Ok(body) = handle_export_spec(&store, &ctx) else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["plan_id"], "exp-json");
        assert_eq!(v["format"], "json");
        let content = v["content"].as_str().expect("content string");
        let parsed: serde_json::Value =
            serde_json::from_str(content).expect("content parses as JSON");
        assert_eq!(parsed["schema_version"], super::super::PLAN_SPEC_VERSION);
        assert_eq!(parsed["spec"]["spec_id"], "exp-json");
        // Signature round-trips → consumer can re-verify.
        let sig = parsed["spec"]["signature"].as_str().expect("signature");
        assert_eq!(sig.len(), 64);
    }

    #[test]
    fn handle_export_spec_markdown_renders_human_summary() {
        let store = super::super::ApprovalStore::open_in_memory().unwrap();
        seed_pending_record(&store, "exp-md");
        let ctx = ctx_with(br#"{"plan_id":"exp-md","format":"markdown"}"#);
        let HandlerOutcome::Ok(body) = handle_export_spec(&store, &ctx) else {
            panic!("expected Ok");
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["format"], "markdown");
        let content = v["content"].as_str().unwrap();
        assert!(content.contains("# Relix Plan exp-md"));
        assert!(content.contains("## Goal"));
        assert!(content.contains("```yaml"));
        assert!(content.contains("Signature (blake3):"));
    }

    #[test]
    fn handle_export_spec_rejects_unknown_format() {
        let store = super::super::ApprovalStore::open_in_memory().unwrap();
        seed_pending_record(&store, "exp-bad");
        let ctx = ctx_with(br#"{"plan_id":"exp-bad","format":"binary"}"#);
        match handle_export_spec(&store, &ctx) {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, relix_core::types::error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("format must be"), "{}", env.cause);
            }
            _ => panic!("expected INVALID_ARGS"),
        }
    }

    #[test]
    fn handle_export_spec_returns_not_found_for_unknown_plan() {
        let store = super::super::ApprovalStore::open_in_memory().unwrap();
        let ctx = ctx_with(br#"{"plan_id":"ghost"}"#);
        match handle_export_spec(&store, &ctx) {
            HandlerOutcome::Err(env) => {
                assert_eq!(env.kind, relix_core::types::error_kinds::INVALID_ARGS);
                assert!(env.cause.contains("not found"));
            }
            _ => panic!("expected INVALID_ARGS"),
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
