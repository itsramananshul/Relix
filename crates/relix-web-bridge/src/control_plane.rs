//! Product-spine overview for the operator control plane.
//!
//! Relix already exposes task, cron, agent, approval, budget, audit,
//! plugin, memory, and policy endpoints. The problem is that those
//! surfaces are scattered by subsystem. This endpoint gives dashboards,
//! CLIs, and operators one canonical map of the durable operating model:
//! tenant -> goal -> agent -> task -> run -> event -> approval/budget.

use axum::{Json, extract::State};
use serde::Serialize;

use crate::config::AppState;

/// `GET /v1/control-plane/spine` response.
#[derive(Debug, Serialize)]
pub struct ControlPlaneSpine {
    pub schema_version: u32,
    pub product: &'static str,
    pub posture: ControlPlanePosture,
    pub runtime: RuntimeReadiness,
    pub surfaces: Vec<SpineSurface>,
}

#[derive(Debug, Serialize)]
pub struct ControlPlanePosture {
    pub tenant_model: &'static str,
    pub work_model: &'static str,
    pub execution_model: &'static str,
    pub approval_model: &'static str,
    pub dashboard_model: &'static str,
}

#[derive(Debug, Serialize)]
pub struct RuntimeReadiness {
    pub coordinator_configured: bool,
    pub task_recorder_available: bool,
    pub mesh_client_initialized: bool,
    pub multi_tenant_mode: bool,
    pub layered_memory_configured: bool,
    pub bridge_started_at: i64,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpineStatus {
    Present,
    Partial,
    #[allow(dead_code)]
    Missing,
}

#[derive(Debug, Serialize)]
pub struct SpineSurface {
    pub id: &'static str,
    pub label: &'static str,
    pub status: SpineStatus,
    pub purpose: &'static str,
    pub routes: &'static [&'static str],
    pub gap: &'static str,
}

/// `GET /v1/control-plane/dashboard` response.
#[derive(Debug, Serialize)]
pub struct DashboardManifest {
    pub schema_version: u32,
    pub source: &'static str,
    pub surfaces: Vec<DashboardSurface>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct DashboardSurface {
    pub id: &'static str,
    pub label: &'static str,
    pub section_id: &'static str,
    pub nav_group: &'static str,
    pub status: SpineStatus,
    pub routes: &'static [&'static str],
    pub gap: &'static str,
}

pub async fn spine(State(state): State<AppState>) -> Json<ControlPlaneSpine> {
    Json(build_spine(&state))
}

pub async fn dashboard_manifest() -> Json<DashboardManifest> {
    Json(build_dashboard_manifest())
}

pub fn build_spine(state: &AppState) -> ControlPlaneSpine {
    ControlPlaneSpine {
        schema_version: 1,
        product: "Relix",
        posture: ControlPlanePosture {
            tenant_model: "tenant header + policy/audit partitioning; still being forced into every route",
            work_model: "task ledger is the canonical work object; chat/capability calls still need tighter task binding",
            execution_model: "attempts/events exist; cooperative cancellation and run ownership are still partial",
            approval_model: "single-call and scoped standing approvals exist; call-count and estimated-cost windows are enforced",
            dashboard_model: "single embedded operator console; useful, but still too monolithic for long-term product work",
        },
        runtime: RuntimeReadiness {
            coordinator_configured: state.cfg.coordinator.is_some(),
            task_recorder_available: state.task_recorder.is_some(),
            mesh_client_initialized: state.mesh_client.is_some(),
            multi_tenant_mode: state.cfg.auth.multi_tenant_mode,
            layered_memory_configured: state.layered_memory.is_some(),
            bridge_started_at: state.started_at,
        },
        surfaces: spine_surfaces(),
    }
}

pub fn spine_surfaces() -> Vec<SpineSurface> {
    vec![
        SpineSurface {
            id: "tenant",
            label: "Tenant / Organization",
            status: SpineStatus::Partial,
            purpose: "Defines the isolation boundary every other object must live inside.",
            routes: &[
                "/v1/policy/tenants",
                "/v1/audit/tenants",
                "/v1/audit/tenants/:tenant_id",
            ],
            gap: "Tenant is still not a mandatory typed input for every bridge route.",
        },
        SpineSurface {
            id: "goals",
            label: "Goals / Planning",
            status: SpineStatus::Partial,
            purpose: "Captures why work exists before agents execute it.",
            routes: &[
                "/v1/planning/plan",
                "/v1/planning/validate",
                "/v1/planning/approvals",
                "/v1/planning/export/:id",
            ],
            gap: "Planning mutations can carry task/run provenance and durable activity; plans are still not a durable first-class goal/project hierarchy.",
        },
        SpineSurface {
            id: "agents",
            label: "Agents",
            status: SpineStatus::Present,
            purpose: "Registers workers with identity, role, token, policy, and approval posture.",
            routes: &[
                "/v1/agents",
                "/v1/agents/:agent_id",
                "/v1/agents/:agent_id/token",
                "/v1/agents/:agent_id/standing-approvals",
            ],
            gap: "Agent records and identity token mutations can carry task/run provenance; budget, workspace, and default runtime binding still need one canonical agent shape.",
        },
        SpineSurface {
            id: "tasks",
            label: "Tasks / Work Items",
            status: SpineStatus::Present,
            purpose: "The central durable unit of work operators and agents can inspect.",
            routes: &[
                "/v1/tasks",
                "/v1/tasks/cursor",
                "/v1/tasks/:id",
                "/v1/tasks/:id/summary",
                "/v1/tasks/:id/todos",
            ],
            gap: "Chat, planning mutations, identity token/research mutations, belief reset, execution rollback, memory-write proxies, knowledge transfer mutations, training export/score/delete mutations, config mutations/tests, MCP invoke, screen-capture, browser-capture, plugin management mutations, standing approval mutations, outbound email, and agent-message send/read/delete paths can attach to a task; remaining direct utility invocations still need forced task or explicit ad-hoc run binding.",
        },
        SpineSurface {
            id: "runs",
            label: "Runs / Attempts",
            status: SpineStatus::Partial,
            purpose: "Tracks execution attempts, retries, lineage, and live event streams.",
            routes: &[
                "/v1/tasks/:id/attempts",
                "/v1/tasks/:id/events",
                "/v1/tasks/:id/events/stream",
                "/v1/tasks/events/stream",
                "/v1/tasks/:id/retry",
                "/v1/tasks/:id/cancel",
            ],
            gap: "Cancellation is still metadata-only for running flows; run ownership needs a stronger runtime protocol.",
        },
        SpineSurface {
            id: "workspaces",
            label: "Execution Workspaces",
            status: SpineStatus::Partial,
            purpose: "Binds an agent run to a concrete filesystem, branch, sandbox, service set, and teardown policy.",
            routes: &[
                "/v1/workspaces",
                "/v1/workspaces/:lease_id",
                "/v1/workspaces/:lease_id/release",
            ],
            gap: "Workspace leases persist ownership, run binding, and command-backed provision/teardown state; sandbox isolation still needs runtime enforcement.",
        },
        SpineSurface {
            id: "schedules",
            label: "Schedules / Heartbeats",
            status: SpineStatus::Partial,
            purpose: "Turns recurring work into tracked task execution.",
            routes: &[
                "/v1/cron/jobs",
                "/v1/cron/jobs/:job_id",
                "/v1/cron/jobs/:job_id/trigger",
            ],
            gap: "Cron exists, but it is not yet presented as agent heartbeat execution with budget/approval preflight.",
        },
        SpineSurface {
            id: "approvals",
            label: "Approvals / Governance",
            status: SpineStatus::Partial,
            purpose: "Stops risky work until an authorized operator or standing policy allows it.",
            routes: &[
                "/v1/approval/pending",
                "/v1/approval/:id",
                "/v1/approval/:id/decision",
                "/v1/approvals",
            ],
            gap: "Task/session/method/workspace scoped approvals exist in the runtime/API with call-count and estimated-cost limits; budget reset now records task-aware governance activity, while dashboard UX and run binding still need launch work.",
        },
        SpineSurface {
            id: "budget",
            label: "Budget / Cost",
            status: SpineStatus::Partial,
            purpose: "Shows and enforces spend limits before agents burn money.",
            routes: &[
                "/v1/budget/status",
                "/v1/metrics/cost",
                "/v1/metrics/cost-baselines",
                "/v1/metrics/cost-spikes",
            ],
            gap: "Cost surfaces exist and budget resets can carry task/run provenance; task/run-level hard-stop semantics need to be more visible and uniform.",
        },
        SpineSurface {
            id: "activity",
            label: "Activity / Audit",
            status: SpineStatus::Partial,
            purpose: "Answers what happened, who did it, and why the system allowed or denied it.",
            routes: &[
                "/v1/activity/recent",
                "/v1/intervention/recent",
                "/v1/policy/denials",
                "/v1/logs/stream",
                "/v1/provenance/recent",
            ],
            gap: "Durable activity JSONL now captures workspace, intervention, approval, standing-approval, budget-reset, identity-token/research, belief-reset, confidence-reset, config mutation/test, policy-denial, cost-observation, planning, execution rollback, memory-write/manual-curation, knowledge-transfer, training export/score/delete, MCP, screen, browser-capture, plugin management, outbound email, and agent-message send/read/delete events; model and dispatch producers still need the same ledger path.",
        },
        SpineSurface {
            id: "memory",
            label: "Memory / Knowledge",
            status: SpineStatus::Present,
            purpose: "Stores long-lived context and agent knowledge.",
            routes: &[
                "/v1/memory/search",
                "/v1/memory/records",
                "/v1/memory/stats",
                "/v1/knowledge/share",
                "/v1/knowledge/broadcast",
                "/v1/knowledge/revoke",
                "/v1/knowledge/recall",
            ],
            gap: "Bridge memory-write proxies, knowledge transfer mutations, and manual curator runs can carry task/run provenance; default binding still needs to expand into every background memory producer.",
        },
        SpineSurface {
            id: "extensions",
            label: "Tools / Plugins / MCP",
            status: SpineStatus::Present,
            purpose: "Allows Relix to discover and invoke external capabilities.",
            routes: &[
                "/v1/tools",
                "/v1/tools/manifest",
                "/v1/plugins",
                "/v1/mcp/servers",
                "/v1/mcp/tools",
            ],
            gap: "MCP, plugin management, and selected tool utilities can carry task context; arbitrary plugin/tool execution still needs mandatory inherited task, tenant, budget, and approval context.",
        },
    ]
}

pub fn build_dashboard_manifest() -> DashboardManifest {
    DashboardManifest {
        schema_version: 1,
        source: "control-plane-spine",
        surfaces: dashboard_manifest_surfaces(),
    }
}

pub fn dashboard_manifest_surfaces() -> Vec<DashboardSurface> {
    let spine = spine_surfaces();
    let lookup = |id: &str| -> SpineSurface {
        spine
            .iter()
            .find(|surface| surface.id == id)
            .map(|surface| SpineSurface {
                id: surface.id,
                label: surface.label,
                status: surface.status,
                purpose: surface.purpose,
                routes: surface.routes,
                gap: surface.gap,
            })
            .unwrap_or_else(|| panic!("dashboard manifest references missing spine surface {id}"))
    };
    let surface =
        |id: &str, section_id: &'static str, nav_group: &'static str| -> DashboardSurface {
            let spine_surface = lookup(id);
            DashboardSurface {
                id: spine_surface.id,
                label: spine_surface.label,
                section_id,
                nav_group,
                status: spine_surface.status,
                routes: spine_surface.routes,
                gap: spine_surface.gap,
            }
        };

    vec![
        surface("tenant", "tenant", "Settings"),
        surface("goals", "planning", "Work"),
        surface("agents", "identity", "Work"),
        surface("tasks", "tasks", "Work"),
        surface("runs", "tasks", "Work"),
        surface("workspaces", "tasks", "Work"),
        surface("schedules", "cron", "Work"),
        surface("approvals", "approvals", "Governance"),
        surface("budget", "cost", "Governance"),
        surface("activity", "observability", "Operations"),
        surface("memory", "memory", "Knowledge"),
        surface("extensions", "plugins", "Extensions"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spine_exposes_the_product_operating_model() {
        let surfaces = spine_surfaces();
        let ids: Vec<&str> = surfaces.iter().map(|s| s.id).collect();
        for required in [
            "tenant",
            "goals",
            "agents",
            "tasks",
            "runs",
            "workspaces",
            "schedules",
            "approvals",
            "budget",
            "activity",
            "memory",
            "extensions",
        ] {
            assert!(ids.contains(&required), "missing spine surface {required}");
        }
    }

    #[test]
    fn every_surface_has_operator_routes_and_an_honest_gap() {
        for surface in spine_surfaces() {
            if surface.status != SpineStatus::Missing {
                assert!(!surface.routes.is_empty(), "{} has no routes", surface.id);
            }
            assert!(!surface.gap.trim().is_empty(), "{} has no gap", surface.id);
            assert!(
                matches!(
                    surface.status,
                    SpineStatus::Present | SpineStatus::Partial | SpineStatus::Missing
                ),
                "{} has invalid status",
                surface.id
            );
        }
    }

    #[test]
    fn task_and_approval_gaps_call_out_the_real_product_failures() {
        let surfaces = spine_surfaces();
        let tasks = surfaces.iter().find(|s| s.id == "tasks").unwrap();
        assert!(tasks.gap.contains("attach to a task"));
        let approvals = surfaces.iter().find(|s| s.id == "approvals").unwrap();
        assert!(approvals.gap.contains("Task/session/method/workspace"));
        assert!(approvals.gap.contains("call-count and estimated-cost"));
    }

    #[test]
    fn dashboard_manifest_is_derived_from_spine_surfaces() {
        let spine_ids: Vec<&str> = spine_surfaces()
            .into_iter()
            .map(|surface| surface.id)
            .collect();
        let manifest = build_dashboard_manifest();
        assert_eq!(manifest.schema_version, 1);
        assert_eq!(manifest.source, "control-plane-spine");

        for surface in manifest.surfaces {
            assert!(
                spine_ids.contains(&surface.id),
                "dashboard surface {} is not in the product spine",
                surface.id
            );
            assert!(
                !surface.section_id.trim().is_empty(),
                "dashboard surface {} has no section",
                surface.id
            );
            assert!(
                !surface.nav_group.trim().is_empty(),
                "dashboard surface {} has no nav group",
                surface.id
            );
        }
    }

    #[test]
    fn dashboard_manifest_maps_spine_to_existing_operator_sections() {
        let manifest = build_dashboard_manifest();
        for (surface_id, expected_section) in [
            ("tenant", "tenant"),
            ("goals", "planning"),
            ("tasks", "tasks"),
            ("approvals", "approvals"),
            ("activity", "observability"),
            ("memory", "memory"),
            ("extensions", "plugins"),
        ] {
            let surface = manifest
                .surfaces
                .iter()
                .find(|surface| surface.id == surface_id)
                .unwrap_or_else(|| panic!("missing dashboard surface {surface_id}"));
            assert_eq!(surface.section_id, expected_section);
        }
    }
}
