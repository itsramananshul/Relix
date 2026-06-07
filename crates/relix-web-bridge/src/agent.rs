//! HTTP proxies for the agent employee permission model.
//!
//! Endpoints (all forward to the coordinator's `agent.*` /
//! `coord.approval.*` / `identity.*` capabilities):
//!
//! - `GET    /v1/agents                                  ` — list.
//! - `POST   /v1/agents                                  ` — create; returns AgentId + issued token.
//! - `GET    /v1/agents/:agent_id                        ` — detail.
//! - `PATCH  /v1/agents/:agent_id                        ` — update one field.
//! - `DELETE /v1/agents/:agent_id                        ` — soft delete (revoke).
//! - `POST   /v1/agents/:agent_id/tokens                 ` — issue a fresh token for an agent.
//! - `GET    /v1/approvals                               ` — pending approvals.
//! - `POST   /v1/approvals/:approval_id/decide           ` — approve / reject.
//! - `GET    /v1/agents/:agent_id/standing-approvals     ` — list standing.
//! - `POST   /v1/agents/:agent_id/standing-approvals     ` — grant.
//! - `DELETE /v1/standing-approvals/:standing_id         ` — revoke.

use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::activity::{ToolInvocationActivity, append_tool_invocation_activity};
use crate::config::AppState;
use crate::tenant::{DEFAULT_TENANT, current_subject};

const DEFAULT_PEER: &str = "coordinator";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

// ── Agent CRUD ───────────────────────────────────────────

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct AgentRow {
    pub agent_id: String,
    pub name: String,
    pub role: String,
    pub status: String,
    pub subject_id: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct AgentListResponse {
    pub agents: Vec<AgentRow>,
    pub count: usize,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    pub subject_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct CreateAgentRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub department: Option<String>,
    #[serde(default)]
    pub team: Option<String>,
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default)]
    pub subject_id: Option<String>,
    #[serde(default)]
    pub risk_ceiling: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateAgentResponse {
    pub agent_id: String,
    /// Session-identity token issued at registration time.
    /// `None` when the coordinator does not have the
    /// `identity.issue_token` capability registered — callers
    /// can still use `POST /v1/agents/:id/tokens` later.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

// ── Token issuance ────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct IssueAgentTokenRequest {
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct AgentTokenResponse {
    pub agent_id: String,
    pub token: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct AgentDetail {
    pub agent_id: String,
    pub name: String,
    pub role: String,
    pub title: String,
    pub department: String,
    pub team: String,
    pub created_by: String,
    pub status: String,
    pub subject_id: String,
    pub risk_ceiling: String,
    pub approval_timeout_secs: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub surface_allowlist: Vec<String>,
    pub allow_categories: Vec<String>,
    pub deny_categories: Vec<String>,
    pub allow_sensitivity_tags: Vec<String>,
    pub deny_sensitivity_tags: Vec<String>,
    pub approval_required_categories: Vec<String>,
    pub rig: Option<String>,
    pub monthly_allowance_cents: Option<i64>,
    pub max_concurrent_runs: i64,
    pub wake_on_timer: bool,
    pub wake_on_demand: bool,
    /// Adapter preference (relix-agent-adapters.md §3.2/§3.3/§7,
    /// relix-dashboard-design.md §9 "model lane"). STORED PREFERENCE
    /// ONLY — adapter execution does not consume it yet.
    pub model_preference: Option<String>,
    /// Adapter preference: reasoning/effort tier. STORED PREFERENCE ONLY.
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateAgentRequest {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub department: Option<String>,
    #[serde(default)]
    pub team: Option<String>,
    #[serde(default)]
    pub risk_ceiling: Option<String>,
    #[serde(default)]
    pub surface_allowlist: Option<String>,
    #[serde(default)]
    pub allow_categories: Option<String>,
    #[serde(default)]
    pub deny_categories: Option<String>,
    #[serde(default)]
    pub allow_sensitivity_tags: Option<String>,
    #[serde(default)]
    pub deny_sensitivity_tags: Option<String>,
    #[serde(default)]
    pub approval_required_categories: Option<String>,
    #[serde(default)]
    pub approval_timeout_secs: Option<i64>,
    #[serde(default)]
    pub rig: Option<String>,
    #[serde(default)]
    pub monthly_allowance_cents: Option<i64>,
    #[serde(default)]
    pub max_concurrent_runs: Option<i64>,
    #[serde(default)]
    pub wake_on_timer: Option<bool>,
    #[serde(default)]
    pub wake_on_demand: Option<bool>,
    // Org/Work Keys (company-model §5.2).
    #[serde(default)]
    pub can_spawn_agents: Option<bool>,
    #[serde(default)]
    pub spawn_route: Option<String>,
    #[serde(default)]
    pub can_assign_work: Option<bool>,
    #[serde(default)]
    pub assign_scope: Option<String>,
    #[serde(default)]
    pub assign_allowed_agents: Option<String>,
    #[serde(default)]
    pub can_manage_work: Option<bool>,
    #[serde(default)]
    pub manage_scope: Option<String>,
    #[serde(default)]
    pub manage_allowed_agents: Option<String>,
    #[serde(default)]
    pub can_configure_agents: Option<bool>,
    #[serde(default)]
    pub configure_scope: Option<String>,
    #[serde(default)]
    pub configure_allowed_agents: Option<String>,
    #[serde(default)]
    pub secret_allowlist: Option<String>,
    #[serde(default)]
    pub instruction_bundle: Option<String>,
    // Adapter preferences (relix-agent-adapters.md §3.2/§3.3/§7). Empty
    // string clears. STORED PREFERENCE ONLY — adapter execution does not
    // consume them yet.
    #[serde(default)]
    pub model_preference: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OkResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

pub async fn list_agents(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<AgentListResponse>, (StatusCode, Json<ApiError>)> {
    let subject = q.subject_id.unwrap_or_default();
    let body = call_peer_string(&state, DEFAULT_PEER, "agent.list", subject.as_bytes()).await?;
    let agents = parse_list_body(&body);
    let count = agents.len();
    Ok(Json(AgentListResponse { agents, count }))
}

pub async fn create_agent(
    State(state): State<AppState>,
    Json(req): Json<CreateAgentRequest>,
) -> Result<Json<CreateAgentResponse>, (StatusCode, Json<ApiError>)> {
    let name = require_field(&req.name, "name")?;
    let role = require_field(&req.role, "role")?;
    let title = require_field(&req.title, "title")?;
    let department = require_field(&req.department, "department")?;
    let team = require_field(&req.team, "team")?;
    let created_by = require_field(&req.created_by, "created_by")?;
    let subject_id = require_field(&req.subject_id, "subject_id")?;
    let risk_ceiling = req
        .risk_ceiling
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("medium")
        .to_string();
    for (label, val) in [
        ("name", name.as_str()),
        ("role", role.as_str()),
        ("title", title.as_str()),
        ("department", department.as_str()),
        ("team", team.as_str()),
        ("created_by", created_by.as_str()),
        ("subject_id", subject_id.as_str()),
        ("risk_ceiling", risk_ceiling.as_str()),
    ] {
        if val.contains('|') {
            return Err(bad(format!("{label} must not contain `|`")));
        }
    }
    let arg = format!(
        "{name}|{role}|{title}|{department}|{team}|{created_by}|{subject_id}|{risk_ceiling}"
    );
    let body = call_peer_string(&state, DEFAULT_PEER, "agent.create", arg.as_bytes()).await?;
    let agent_id = body.trim().to_string();
    let token = try_issue_agent_token(&state, &agent_id, &name, &[], None).await;
    Ok(Json(CreateAgentResponse { agent_id, token }))
}

pub async fn issue_agent_token(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<IssueAgentTokenRequest>,
) -> Result<Json<AgentTokenResponse>, (StatusCode, Json<ApiError>)> {
    let detail_body =
        call_peer_string(&state, DEFAULT_PEER, "agent.get", agent_id.as_bytes()).await?;
    let detail = parse_agent_detail(&detail_body).ok_or((
        StatusCode::BAD_GATEWAY,
        Json(ApiError {
            error: format!("agent.get returned an unparseable body: {detail_body:?}"),
        }),
    ))?;
    let token = try_issue_agent_token(&state, &agent_id, &detail.name, &req.scopes, req.ttl_secs)
        .await
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ApiError {
                error: "identity.issue_token capability is not available on this deployment".into(),
            }),
        ))?;
    Ok(Json(AgentTokenResponse { agent_id, token }))
}

pub async fn get_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentDetail>, (StatusCode, Json<ApiError>)> {
    let body = call_peer_string(&state, DEFAULT_PEER, "agent.get", agent_id.as_bytes()).await?;
    let parsed = parse_agent_detail(&body).ok_or((
        StatusCode::BAD_GATEWAY,
        Json(ApiError {
            error: format!("agent.get returned an unparseable body: {body:?}"),
        }),
    ))?;
    Ok(Json(parsed))
}

pub async fn update_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<UpdateAgentRequest>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let mut applied = false;
    let apply_field = |field: &str, value: &str| -> Result<(), (StatusCode, Json<ApiError>)> {
        if value.contains('|') {
            return Err(bad(format!("{field} must not contain `|`")));
        }
        Ok(())
    };

    let mut commits: Vec<(String, String)> = Vec::new();
    if let Some(v) = req.status {
        apply_field("status", &v)?;
        commits.push(("status".into(), v));
    }
    if let Some(v) = req.role {
        apply_field("role", &v)?;
        commits.push(("role".into(), v));
    }
    if let Some(v) = req.title {
        apply_field("title", &v)?;
        commits.push(("title".into(), v));
    }
    if let Some(v) = req.department {
        apply_field("department", &v)?;
        commits.push(("department".into(), v));
    }
    if let Some(v) = req.team {
        apply_field("team", &v)?;
        commits.push(("team".into(), v));
    }
    if let Some(v) = req.risk_ceiling {
        apply_field("risk_ceiling", &v)?;
        commits.push(("risk_ceiling".into(), v));
    }
    if let Some(v) = req.surface_allowlist {
        commits.push(("surface_allowlist".into(), v));
    }
    if let Some(v) = req.allow_categories {
        commits.push(("allow_categories".into(), v));
    }
    if let Some(v) = req.deny_categories {
        commits.push(("deny_categories".into(), v));
    }
    if let Some(v) = req.allow_sensitivity_tags {
        commits.push(("allow_sensitivity_tags".into(), v));
    }
    if let Some(v) = req.deny_sensitivity_tags {
        commits.push(("deny_sensitivity_tags".into(), v));
    }
    if let Some(v) = req.approval_required_categories {
        commits.push(("approval_required_categories".into(), v));
    }
    if let Some(v) = req.approval_timeout_secs {
        commits.push(("approval_timeout_secs".into(), v.to_string()));
    }
    if let Some(v) = req.rig {
        commits.push(("rig".into(), v));
    }
    if let Some(v) = req.monthly_allowance_cents {
        commits.push(("monthly_allowance_cents".into(), v.to_string()));
    }
    if let Some(v) = req.max_concurrent_runs {
        commits.push(("max_concurrent_runs".into(), v.to_string()));
    }
    if let Some(v) = req.wake_on_timer {
        commits.push(("wake_on_timer".into(), v.to_string()));
    }
    if let Some(v) = req.wake_on_demand {
        commits.push(("wake_on_demand".into(), v.to_string()));
    }
    // Org/Work Keys (company-model §5.2). The runtime validates enum
    // values and list shapes; the bridge just forwards the edit.
    if let Some(v) = req.can_spawn_agents {
        commits.push(("can_spawn_agents".into(), v.to_string()));
    }
    if let Some(v) = req.spawn_route {
        commits.push(("spawn_route".into(), v));
    }
    if let Some(v) = req.can_assign_work {
        commits.push(("can_assign_work".into(), v.to_string()));
    }
    if let Some(v) = req.assign_scope {
        commits.push(("assign_scope".into(), v));
    }
    if let Some(v) = req.assign_allowed_agents {
        commits.push(("assign_allowed_agents".into(), v));
    }
    if let Some(v) = req.can_manage_work {
        commits.push(("can_manage_work".into(), v.to_string()));
    }
    if let Some(v) = req.manage_scope {
        commits.push(("manage_scope".into(), v));
    }
    if let Some(v) = req.manage_allowed_agents {
        commits.push(("manage_allowed_agents".into(), v));
    }
    if let Some(v) = req.can_configure_agents {
        commits.push(("can_configure_agents".into(), v.to_string()));
    }
    if let Some(v) = req.configure_scope {
        commits.push(("configure_scope".into(), v));
    }
    if let Some(v) = req.configure_allowed_agents {
        commits.push(("configure_allowed_agents".into(), v));
    }
    if let Some(v) = req.secret_allowlist {
        commits.push(("secret_allowlist".into(), v));
    }
    if let Some(v) = req.instruction_bundle {
        commits.push(("instruction_bundle".into(), v));
    }
    // Adapter preferences (relix-agent-adapters.md §3.2/§3.3/§7). The runtime
    // validates the effort enum + length-caps the model name; empty clears.
    if let Some(v) = req.model_preference {
        commits.push(("model_preference".into(), v));
    }
    if let Some(v) = req.reasoning_effort {
        commits.push(("reasoning_effort".into(), v));
    }

    if commits.is_empty() {
        return Err(bad("at least one updatable field required".into()));
    }
    for (field, value) in commits {
        let arg = format!("{agent_id}|{field}|{value}");
        let _ = call_peer_string(&state, DEFAULT_PEER, "agent.update", arg.as_bytes()).await?;
        applied = true;
    }
    let _ = applied;
    Ok(Json(OkResponse {
        ok: true,
        task_id: None,
        run_id: None,
    }))
}

pub async fn delete_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let _ = call_peer_string(&state, DEFAULT_PEER, "agent.delete", agent_id.as_bytes()).await?;
    Ok(Json(OkResponse {
        ok: true,
        task_id: None,
        run_id: None,
    }))
}

/// Optional JSON body for `POST /v1/agents/:agent_id/approve-hire`. The
/// body (and `rig`) are optional — an empty request keeps the legacy
/// "approve, no Rig" behaviour; supplying `rig` makes the approved
/// Operative immediately runnable (company-model §12.6). For the
/// safe-local first-run path that Rig is `echo`.
#[derive(Debug, Deserialize, Default)]
pub struct ApproveHireRequest {
    #[serde(default)]
    pub rig: Option<String>,
}

/// The coordinator's `agent.approve_hire` JSON result.
#[derive(Debug, Deserialize)]
struct ApproveHireWire {
    #[serde(default)]
    rig: Option<String>,
    #[serde(default)]
    rig_set: bool,
    #[serde(default)]
    runnable: bool,
    #[serde(default)]
    needs_rig: bool,
}

/// Response for `POST /v1/agents/:agent_id/approve-hire`. Tells the client
/// whether the now-active Operative is runnable, and — when it is not —
/// makes the required follow-up machine-actionable (`needs_rig`).
#[derive(Debug, Serialize)]
pub struct ApproveHireResponse {
    pub ok: bool,
    pub agent_id: String,
    /// The Rig bound to the Operative after approval (omitted when none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rig: Option<String>,
    /// Did this approval assign the Rig?
    pub rig_set: bool,
    /// Can a Shift dispatch to this Operative now?
    pub runnable: bool,
    /// The Operative still needs a Rig configured before it can run.
    pub needs_rig: bool,
}

/// `POST /v1/agents/:agent_id/approve-hire` — greenlight a `pending` hire
/// (pending → active), the governed affordance the Action Center's "Approve
/// the hire" item points at (a `route=direct`/Prime pending hire carries no
/// spawn Clearance, so it is activated here rather than via
/// `/v1/approvals/.../decide`). Forwards to the coordinator's owner-gated
/// `agent.approve_hire`; the runtime enforces the gate + the pending-only
/// transition.
///
/// Accepts an **optional** JSON body `{"rig":"echo"}`: when present, the Rig
/// is validated + bound atomically at approval so the approved Operative is
/// **immediately runnable** without a separate `PATCH /v1/agents/:id {rig}`
/// (company-model §12.6). With no body / no `rig`, behaviour is unchanged and
/// the response's `needs_rig` flag tells the client a Rig must still be
/// configured. The request body must be empty or valid JSON.
pub async fn approve_hire(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    body: Bytes,
) -> Result<Json<ApproveHireResponse>, (StatusCode, Json<ApiError>)> {
    // The body is optional: empty ⇒ legacy "approve, no Rig". Only parse when
    // bytes are present so no-body callers keep working.
    let req: ApproveHireRequest = if body.is_empty() {
        ApproveHireRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| bad(format!("invalid JSON body: {e}")))?
    };
    let rig = req.rig.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(r) = rig
        && r.contains('|')
    {
        return Err(bad("rig must not contain `|`".into()));
    }
    // Wire arg: `agent_id` or `agent_id|rig` (the runtime validates the Rig
    // against the known-Rig allowlist + enforces the pending-only transition).
    let arg = match rig {
        Some(r) => format!("{agent_id}|{r}"),
        None => agent_id.clone(),
    };
    let resp = call_peer_string(&state, DEFAULT_PEER, "agent.approve_hire", arg.as_bytes()).await?;
    let wire: ApproveHireWire = serde_json::from_str(resp.trim()).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("agent.approve_hire returned an unparseable body: {resp:?} ({e})"),
            }),
        )
    })?;
    Ok(Json(ApproveHireResponse {
        ok: true,
        agent_id,
        rig: wire.rig,
        rig_set: wire.rig_set,
        runnable: wire.runnable,
        needs_rig: wire.needs_rig,
    }))
}

/// `POST /v1/agents/:agent_id/reject-hire` — decline a `pending` hire
/// (pending → disabled). Forwards to the owner-gated `agent.reject_hire`.
pub async fn reject_hire(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let _ = call_peer_string(
        &state,
        DEFAULT_PEER,
        "agent.reject_hire",
        agent_id.as_bytes(),
    )
    .await?;
    Ok(Json(OkResponse {
        ok: true,
        task_id: None,
        run_id: None,
    }))
}

// ── Approvals ────────────────────────────────────────────

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PendingApprovalRow {
    pub approval_id: String,
    pub agent_id: String,
    pub method: String,
    pub reason: String,
    pub requested_at: i64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PendingApprovalsResponse {
    pub approvals: Vec<PendingApprovalRow>,
    pub count: usize,
}

#[derive(Debug, Deserialize, Default)]
pub struct DecideRequest {
    pub decision: String,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub decided_by: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DecideResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_token: Option<String>,
}

pub async fn pending_approvals(
    State(state): State<AppState>,
) -> Result<Json<PendingApprovalsResponse>, (StatusCode, Json<ApiError>)> {
    let body = call_peer_string(&state, DEFAULT_PEER, "coord.approval.pending", b"").await?;
    let approvals = parse_pending_body(&body);
    let count = approvals.len();
    Ok(Json(PendingApprovalsResponse { approvals, count }))
}

pub async fn decide_approval(
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    Json(req): Json<DecideRequest>,
) -> Result<Json<DecideResponse>, (StatusCode, Json<ApiError>)> {
    if !matches!(req.decision.as_str(), "approved" | "rejected") {
        return Err(bad(format!(
            "decision must be `approved` or `rejected`, got `{}`",
            req.decision
        )));
    }
    let note = req.note.unwrap_or_default();
    let decided_by = req.decided_by.unwrap_or_else(|| "operator".to_string());
    let arg = format!("{approval_id}|{}|{decided_by}|{note}", req.decision);
    let body = call_peer_string(
        &state,
        DEFAULT_PEER,
        "coord.approval.decide",
        arg.as_bytes(),
    )
    .await?;
    // body is `ok\n` or `ok|<token>\n`.
    let trimmed = body.trim();
    let token = trimmed
        .strip_prefix("ok|")
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    Ok(Json(DecideResponse {
        ok: true,
        approval_token: token,
    }))
}

// ── Standing approvals ───────────────────────────────────

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct StandingRow {
    pub standing_id: String,
    pub match_category: String,
    pub match_path_glob: Option<String>,
    pub scope_kind: String,
    pub task_id: Option<String>,
    pub session_id: Option<String>,
    pub method_prefix: Option<String>,
    pub workspace_path_glob: Option<String>,
    pub expires_at: i64,
    pub granted_by: String,
    pub max_calls: Option<i64>,
    pub calls_used: i64,
    pub max_cost_micros: Option<i64>,
    pub cost_used_micros: i64,
    pub note: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct StandingListResponse {
    pub standing: Vec<StandingRow>,
    pub count: usize,
}

#[derive(Debug, Deserialize, Default)]
pub struct StandingCreateRequest {
    pub category: String,
    pub expires_at: i64,
    #[serde(default)]
    pub granted_by: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub path_glob: Option<String>,
    #[serde(default)]
    pub scope_kind: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub method_prefix: Option<String>,
    #[serde(default)]
    pub workspace_path_glob: Option<String>,
    #[serde(default)]
    pub max_calls: Option<i64>,
    #[serde(default)]
    pub max_cost_micros: Option<i64>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct StandingCreateForward<'a> {
    agent_id: &'a str,
    category: &'a str,
    expires_at: i64,
    granted_by: &'a str,
    note: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    path_glob: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    method_prefix: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_path_glob: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_calls: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_cost_micros: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct StandingCreateResponse {
    pub standing_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

pub async fn list_standing(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<StandingListResponse>, (StatusCode, Json<ApiError>)> {
    let body = call_peer_string(
        &state,
        DEFAULT_PEER,
        "agent.standing_approval.list",
        agent_id.as_bytes(),
    )
    .await?;
    let standing = parse_standing_body(&body);
    let count = standing.len();
    Ok(Json(StandingListResponse { standing, count }))
}

pub async fn create_standing(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
    Json(req): Json<StandingCreateRequest>,
) -> Result<Json<StandingCreateResponse>, (StatusCode, Json<ApiError>)> {
    if req.category.trim().is_empty() {
        return Err(bad("category required".into()));
    }
    if req.expires_at <= 0 {
        return Err(bad("expires_at must be a positive unix timestamp".into()));
    }
    if req.max_calls.is_some_and(|n| n <= 0) {
        return Err(bad("max_calls must be positive when provided".into()));
    }
    if req.max_cost_micros.is_some_and(|n| n <= 0) {
        return Err(bad("max_cost_micros must be positive when provided".into()));
    }
    let task_id = clean_optional(req.task_id.as_deref());
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = standing_create_detail(
        &agent_id,
        &req,
        req.note.as_ref().map(|s| s.len()).unwrap_or_default(),
    );
    let granted_by = req
        .granted_by
        .clone()
        .unwrap_or_else(|| "operator".to_string());
    let note = req.note.clone().unwrap_or_default();
    let forward = StandingCreateForward {
        agent_id: &agent_id,
        category: &req.category,
        expires_at: req.expires_at,
        granted_by: &granted_by,
        note: &note,
        path_glob: req.path_glob.as_deref(),
        scope_kind: req.scope_kind.as_deref(),
        task_id: req.task_id.as_deref(),
        session_id: req.session_id.as_deref(),
        method_prefix: req.method_prefix.as_deref(),
        workspace_path_glob: req.workspace_path_glob.as_deref(),
        max_calls: req.max_calls,
        max_cost_micros: req.max_cost_micros,
    };
    let arg = serde_json::to_vec(&forward).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: format!("standing approval encode failed: {e}"),
            }),
        )
    })?;
    let body = match call_peer_string_scoped(
        &state,
        DEFAULT_PEER,
        "agent.standing_approval.create",
        &arg,
        task_id.as_deref(),
    )
    .await
    {
        Ok(body) => {
            record_standing_activity(
                &state,
                StandingActivity {
                    actor: &granted_by,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "agent.standing_approval.create",
                    decision: "ok",
                    detail: &detail,
                },
            );
            body
        }
        Err(err) => {
            record_standing_activity(
                &state,
                StandingActivity {
                    actor: &granted_by,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "agent.standing_approval.create",
                    decision: "err",
                    detail: &detail,
                },
            );
            return Err(err);
        }
    };
    Ok(Json(StandingCreateResponse {
        standing_id: body.trim().to_string(),
        task_id,
        run_id,
    }))
}

#[derive(Debug, Deserialize, Default)]
pub struct ScopeQuery {
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

pub async fn revoke_standing(
    State(state): State<AppState>,
    Path(standing_id): Path<String>,
    Query(q): Query<ScopeQuery>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let task_id = clean_optional(q.task_id.as_deref());
    let run_id = clean_optional(q.run_id.as_deref());
    let detail = standing_revoke_detail(&standing_id);
    let actor = current_subject().unwrap_or_else(|| "operator".to_string());
    match call_peer_string_scoped(
        &state,
        DEFAULT_PEER,
        "agent.standing_approval.revoke",
        standing_id.as_bytes(),
        task_id.as_deref(),
    )
    .await
    {
        Ok(_) => {
            record_standing_activity(
                &state,
                StandingActivity {
                    actor: &actor,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "agent.standing_approval.revoke",
                    decision: "ok",
                    detail: &detail,
                },
            );
            Ok(Json(OkResponse {
                ok: true,
                task_id,
                run_id,
            }))
        }
        Err(err) => {
            record_standing_activity(
                &state,
                StandingActivity {
                    actor: &actor,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "agent.standing_approval.revoke",
                    decision: "err",
                    detail: &detail,
                },
            );
            Err(err)
        }
    }
}

// ── Parsers ──────────────────────────────────────────────

pub fn parse_list_body(body: &str) -> Vec<AgentRow> {
    body.lines()
        .filter(|line| !line.starts_with("count=") && !line.trim().is_empty())
        .filter_map(|line| {
            let cols: Vec<&str> = line.splitn(5, '\t').collect();
            if cols.len() != 5 {
                return None;
            }
            Some(AgentRow {
                agent_id: cols[0].into(),
                name: cols[1].into(),
                role: cols[2].into(),
                status: cols[3].into(),
                subject_id: cols[4].into(),
            })
        })
        .collect()
}

pub fn parse_agent_detail(body: &str) -> Option<AgentDetail> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = AgentDetail {
        agent_id: String::new(),
        name: String::new(),
        role: String::new(),
        title: String::new(),
        department: String::new(),
        team: String::new(),
        created_by: String::new(),
        status: String::new(),
        subject_id: String::new(),
        risk_ceiling: String::new(),
        approval_timeout_secs: 0,
        created_at: 0,
        updated_at: 0,
        surface_allowlist: vec![],
        allow_categories: vec![],
        deny_categories: vec![],
        allow_sensitivity_tags: vec![],
        deny_sensitivity_tags: vec![],
        approval_required_categories: vec![],
        rig: None,
        monthly_allowance_cents: None,
        max_concurrent_runs: 20,
        wake_on_timer: true,
        wake_on_demand: true,
        model_preference: None,
        reasoning_effort: None,
    };
    for kv in trimmed.split('|') {
        let (k, v) = kv.split_once('=')?;
        match k.trim() {
            "agent_id" => out.agent_id = v.into(),
            "name" => out.name = v.into(),
            "role" => out.role = v.into(),
            "title" => out.title = v.into(),
            "department" => out.department = v.into(),
            "team" => out.team = v.into(),
            "created_by" => out.created_by = v.into(),
            "status" => out.status = v.into(),
            "subject_id" => out.subject_id = v.into(),
            "risk_ceiling" => out.risk_ceiling = v.into(),
            "approval_timeout_secs" => out.approval_timeout_secs = v.trim().parse().ok()?,
            "created_at" => out.created_at = v.trim().parse().ok()?,
            "updated_at" => out.updated_at = v.trim().parse().ok()?,
            "surface_allowlist" => out.surface_allowlist = parse_csv(v),
            "allow_categories" => out.allow_categories = parse_csv(v),
            "deny_categories" => out.deny_categories = parse_csv(v),
            "allow_sensitivity_tags" => out.allow_sensitivity_tags = parse_csv(v),
            "deny_sensitivity_tags" => out.deny_sensitivity_tags = parse_csv(v),
            "approval_required_categories" => out.approval_required_categories = parse_csv(v),
            "rig" => out.rig = opt_string(v),
            "monthly_allowance_cents" => out.monthly_allowance_cents = v.trim().parse().ok(),
            "max_concurrent_runs" => out.max_concurrent_runs = v.trim().parse().ok()?,
            "wake_on_timer" => out.wake_on_timer = parse_bool_wire(v)?,
            "wake_on_demand" => out.wake_on_demand = parse_bool_wire(v)?,
            "model_preference" => out.model_preference = opt_string(v),
            "reasoning_effort" => out.reasoning_effort = opt_string(v),
            _ => {}
        }
    }
    Some(out)
}

pub fn parse_pending_body(body: &str) -> Vec<PendingApprovalRow> {
    body.lines()
        .filter(|line| !line.starts_with("count=") && !line.trim().is_empty())
        .filter_map(|line| {
            let cols: Vec<&str> = line.splitn(5, '\t').collect();
            if cols.len() != 5 {
                return None;
            }
            Some(PendingApprovalRow {
                approval_id: cols[0].into(),
                agent_id: cols[1].into(),
                method: cols[2].into(),
                reason: cols[3].into(),
                requested_at: cols[4].parse().ok()?,
            })
        })
        .collect()
}

pub fn parse_standing_body(body: &str) -> Vec<StandingRow> {
    body.lines()
        .filter(|line| !line.starts_with("count=") && !line.trim().is_empty())
        .filter_map(|line| {
            let cols: Vec<&str> = line.splitn(15, '\t').collect();
            if cols.len() == 6 {
                return Some(StandingRow {
                    standing_id: cols[0].into(),
                    match_category: cols[1].into(),
                    match_path_glob: opt_string(cols[2]),
                    scope_kind: "agent_category".into(),
                    task_id: None,
                    session_id: None,
                    method_prefix: None,
                    workspace_path_glob: None,
                    expires_at: cols[3].parse().ok()?,
                    granted_by: cols[4].into(),
                    max_calls: None,
                    calls_used: 0,
                    max_cost_micros: None,
                    cost_used_micros: 0,
                    note: cols[5].into(),
                });
            }
            if cols.len() == 11 {
                return Some(StandingRow {
                    standing_id: cols[0].into(),
                    match_category: cols[1].into(),
                    match_path_glob: opt_string(cols[2]),
                    scope_kind: cols[3].into(),
                    task_id: opt_string(cols[4]),
                    session_id: opt_string(cols[5]),
                    method_prefix: opt_string(cols[6]),
                    workspace_path_glob: opt_string(cols[7]),
                    expires_at: cols[8].parse().ok()?,
                    granted_by: cols[9].into(),
                    max_calls: None,
                    calls_used: 0,
                    max_cost_micros: None,
                    cost_used_micros: 0,
                    note: cols[10].into(),
                });
            }
            if cols.len() == 13 {
                return Some(StandingRow {
                    standing_id: cols[0].into(),
                    match_category: cols[1].into(),
                    match_path_glob: opt_string(cols[2]),
                    scope_kind: cols[3].into(),
                    task_id: opt_string(cols[4]),
                    session_id: opt_string(cols[5]),
                    method_prefix: opt_string(cols[6]),
                    workspace_path_glob: opt_string(cols[7]),
                    expires_at: cols[8].parse().ok()?,
                    granted_by: cols[9].into(),
                    max_calls: cols[10].parse().ok(),
                    calls_used: cols[11].parse().ok()?,
                    max_cost_micros: None,
                    cost_used_micros: 0,
                    note: cols[12].into(),
                });
            }
            if cols.len() != 15 {
                return None;
            }
            Some(StandingRow {
                standing_id: cols[0].into(),
                match_category: cols[1].into(),
                match_path_glob: opt_string(cols[2]),
                scope_kind: cols[3].into(),
                task_id: opt_string(cols[4]),
                session_id: opt_string(cols[5]),
                method_prefix: opt_string(cols[6]),
                workspace_path_glob: opt_string(cols[7]),
                expires_at: cols[8].parse().ok()?,
                granted_by: cols[9].into(),
                max_calls: cols[10].parse().ok(),
                calls_used: cols[11].parse().ok()?,
                max_cost_micros: cols[12].parse().ok(),
                cost_used_micros: cols[13].parse().ok()?,
                note: cols[14].into(),
            })
        })
        .collect()
}

fn opt_string(s: &str) -> Option<String> {
    if s.is_empty() { None } else { Some(s.into()) }
}

fn parse_csv(s: &str) -> Vec<String> {
    if s.trim().is_empty() {
        return vec![];
    }
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

// ── Helpers (shared with cron / delegate) ────────────────

fn parse_bool_wire(s: &str) -> Option<bool> {
    match s.trim() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn require_field(v: &Option<String>, name: &str) -> Result<String, (StatusCode, Json<ApiError>)> {
    let s = v.as_deref().unwrap_or("").trim();
    if s.is_empty() {
        return Err(bad(format!("{name} is required")));
    }
    Ok(s.to_string())
}

fn bad(msg: String) -> (StatusCode, Json<ApiError>) {
    (StatusCode::BAD_REQUEST, Json(ApiError { error: msg }))
}

fn clean_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn standing_create_detail(agent_id: &str, req: &StandingCreateRequest, note_len: usize) -> String {
    format!(
        "agent_id={agent_id}; category={}; scope_kind={}; expires_at={}; max_calls={}; max_cost_micros={}; note_len={note_len}",
        req.category,
        req.scope_kind.as_deref().unwrap_or("agent_category"),
        req.expires_at,
        req.max_calls
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into()),
        req.max_cost_micros
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into())
    )
}

fn standing_revoke_detail(standing_id: &str) -> String {
    format!("standing_id={standing_id}")
}

struct StandingActivity<'a> {
    actor: &'a str,
    task_id: Option<&'a str>,
    run_id: Option<&'a str>,
    method: &'a str,
    decision: &'a str,
    detail: &'a str,
}

fn record_standing_activity(state: &AppState, activity: StandingActivity<'_>) {
    let tenant_id = crate::tenant::current_tenant_or_none()
        .as_deref()
        .unwrap_or(DEFAULT_TENANT)
        .to_string();
    if let Err(e) = append_tool_invocation_activity(
        state.cfg.transport.data_dir.as_deref(),
        ToolInvocationActivity {
            tenant_id: &tenant_id,
            actor: activity.actor,
            peer: DEFAULT_PEER,
            method: activity.method,
            task_id: activity.task_id,
            run_id: activity.run_id,
            decision: activity.decision,
            detail: activity.detail,
        },
    ) {
        tracing::warn!(
            error = %e,
            method = activity.method,
            "failed to append standing approval activity"
        );
    }
}

async fn call_peer_string(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
) -> Result<String, (StatusCode, Json<ApiError>)> {
    call_peer_string_scoped(state, alias, method, arg, None).await
}

async fn call_peer_string_scoped(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
    task_id: Option<&str>,
) -> Result<String, (StatusCode, Json<ApiError>)> {
    let mesh = state.mesh_client.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: "bridge mesh client not initialized".into(),
        }),
    ))?;
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 60);
    let envelope = build_request_with_tenant(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        deadline_secs,
        None,
        None,
        task_id.map(str::to_string),
        crate::tenant::current_tenant_or_none(),
    );
    let resp_bytes = mesh.call(alias, envelope).await.map_err(|e| {
        let msg = e.to_string();
        let lower = msg.to_ascii_lowercase();
        let status = if lower.contains("unknown alias") || lower.contains("no peer") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_GATEWAY
        };
        (status, Json(ApiError { error: msg }))
    })?;
    let resp = decode_response(&resp_bytes).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("decode response: {e}"),
            }),
        )
    })?;
    match resp.res {
        ResponseResult::Ok(body) => String::from_utf8(body.to_vec()).map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: format!("response body utf8: {e}"),
                }),
            )
        }),
        ResponseResult::Err(env) => {
            let cause = env.cause;
            let lower = cause.to_ascii_lowercase();
            let status = if lower.contains("not found") {
                StatusCode::NOT_FOUND
            } else if env.kind == 5 {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            Err((
                status,
                Json(ApiError {
                    error: format!("responder err kind={} cause={cause}", env.kind),
                }),
            ))
        }
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response from coordinator".into(),
            }),
        )),
    }
}

/// Attempt to issue a session-identity token for `agent_id` /
/// `agent_name` by calling the coordinator's
/// `identity.issue_token` cap. Returns `None` when the cap is
/// not registered on the coordinator (clean degradation) — the
/// agent profile is still created; the caller can retry via
/// `POST /v1/agents/:id/tokens` once the identity service is
/// enabled.
async fn try_issue_agent_token(
    state: &AppState,
    agent_id: &str,
    agent_name: &str,
    scopes: &[String],
    ttl_secs: Option<u64>,
) -> Option<String> {
    let mut body = serde_json::Map::new();
    body.insert("session_id".into(), Value::from(agent_id));
    body.insert("agent_name".into(), Value::from(agent_name));
    body.insert("scopes".into(), Value::from(scopes.to_vec()));
    if let Some(ttl) = ttl_secs {
        body.insert("ttl_secs".into(), Value::from(ttl));
    }
    let resp = call_peer_json(
        state,
        DEFAULT_PEER,
        "identity.issue_token",
        &Value::Object(body),
    )
    .await
    .ok()?;
    resp.get("wire")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
}

async fn call_peer_json(
    state: &AppState,
    alias: &str,
    method: &str,
    args: &Value,
) -> Result<Value, (StatusCode, Json<ApiError>)> {
    let mesh = state.mesh_client.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: "bridge mesh client not initialized".into(),
        }),
    ))?;
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 60);
    let arg_bytes = serde_json::to_vec(args).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: format!("encode args: {e}"),
            }),
        )
    })?;
    let envelope = build_request_with_tenant(
        method,
        arg_bytes,
        state.identity_bundle.clone(),
        deadline_secs,
        None,
        None,
        None,
        crate::tenant::current_tenant_or_none(),
    );
    let resp_bytes = mesh.call(alias, envelope).await.map_err(|e| {
        let msg = e.to_string();
        let lower = msg.to_ascii_lowercase();
        let status = if lower.contains("unknown alias") || lower.contains("no peer") {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::BAD_GATEWAY
        };
        (status, Json(ApiError { error: msg }))
    })?;
    let resp = decode_response(&resp_bytes).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("decode response: {e}"),
            }),
        )
    })?;
    match resp.res {
        ResponseResult::Ok(body) => {
            let text = String::from_utf8(body.to_vec()).map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(ApiError {
                        error: format!("response body utf8: {e}"),
                    }),
                )
            })?;
            serde_json::from_str::<Value>(&text).map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(ApiError {
                        error: format!("response body not JSON: {e}"),
                    }),
                )
            })
        }
        ResponseResult::Err(env) => {
            let status = if env.kind == relix_core::types::error_kinds::INVALID_ARGS {
                StatusCode::BAD_REQUEST
            } else if env.kind == relix_core::types::error_kinds::SECURITY_DENIED {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::BAD_GATEWAY
            };
            Err((
                status,
                Json(ApiError {
                    error: format!("responder err kind={} cause={}", env.kind, env.cause),
                }),
            ))
        }
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_two_rows_with_count_line() {
        let body =
            "id1\tAlice\tresearch\tactive\tsubj-1\nid2\tBob\tfiling\tdisabled\tsubj-2\ncount=2\n";
        let v = parse_list_body(body);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].agent_id, "id1");
        assert_eq!(v[1].status, "disabled");
    }

    #[test]
    fn parse_list_empty_body_returns_empty() {
        assert!(parse_list_body("count=0\n").is_empty());
    }

    #[test]
    fn parse_agent_detail_round_trips_every_field() {
        let body = "agent_id=id1|name=Alice|role=research|title=Junior|department=rd|team=ops|created_by=alice|status=active|subject_id=subj-1|risk_ceiling=medium|approval_timeout_secs=86400|created_at=100|updated_at=200|surface_allowlist=telegram,openwebui|allow_categories=browser,fetch|deny_categories=payments|allow_sensitivity_tags=|deny_sensitivity_tags=credentials:read|approval_required_categories=payments,production_deploy|rig=codex|monthly_allowance_cents=25000|max_concurrent_runs=3|wake_on_timer=false|wake_on_demand=true|model_preference=gpt-5-codex|reasoning_effort=high\n";
        let d = parse_agent_detail(body).unwrap();
        assert_eq!(d.agent_id, "id1");
        assert_eq!(d.allow_categories, vec!["browser", "fetch"]);
        assert_eq!(d.deny_sensitivity_tags, vec!["credentials:read"]);
        assert_eq!(d.surface_allowlist, vec!["telegram", "openwebui"]);
        assert_eq!(d.rig.as_deref(), Some("codex"));
        assert_eq!(d.monthly_allowance_cents, Some(25000));
        assert_eq!(d.max_concurrent_runs, 3);
        assert!(!d.wake_on_timer);
        assert!(d.wake_on_demand);
        assert_eq!(d.model_preference.as_deref(), Some("gpt-5-codex"));
        assert_eq!(d.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn parse_agent_detail_model_prefs_absent_default_none() {
        // A legacy detail body (no model preference fields) leaves both None.
        let body = "agent_id=id1|name=Alice|role=research|title=Junior|department=rd|team=ops|created_by=alice|status=active|subject_id=subj-1|risk_ceiling=medium|approval_timeout_secs=86400|created_at=100|updated_at=200|rig=echo|max_concurrent_runs=20|wake_on_timer=true|wake_on_demand=true\n";
        let d = parse_agent_detail(body).unwrap();
        assert_eq!(d.model_preference, None);
        assert_eq!(d.reasoning_effort, None);
    }

    #[test]
    fn update_request_parses_model_prefs_and_empty_clears() {
        let req: UpdateAgentRequest =
            serde_json::from_str(r#"{"model_preference":"claude-sonnet-4","reasoning_effort":""}"#)
                .unwrap();
        assert_eq!(req.model_preference.as_deref(), Some("claude-sonnet-4"));
        // An explicit empty string is preserved (it CLEARS the field downstream),
        // distinct from an absent field (None — left untouched).
        assert_eq!(req.reasoning_effort.as_deref(), Some(""));
        let absent: UpdateAgentRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.model_preference, None);
        assert_eq!(absent.reasoning_effort, None);
    }

    #[test]
    fn parse_pending_two_rows_with_count_line() {
        let body = "apr-1\tagt-1\ttool.x\twhy\t100\napr-2\tagt-2\ttool.y\tcause\t200\ncount=2\n";
        let v = parse_pending_body(body);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].approval_id, "apr-1");
        assert_eq!(v[1].method, "tool.y");
    }

    #[test]
    fn parse_standing_returns_optional_path_glob() {
        let body = "std-1\tfs\t/inbox/**\t9999\talice\tmonthly\nstd-2\tbrowser\t\t8888\talice\t\ncount=2\n";
        let v = parse_standing_body(body);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].match_path_glob.as_deref(), Some("/inbox/**"));
        assert_eq!(v[1].match_path_glob, None);
    }

    // ── CreateAgentResponse serialisation ───────────────────

    #[test]
    fn parse_scoped_standing_returns_scope_fields() {
        let body = "std-1\tbrowser\t\tmethod_prefix\t\t\ttool.web_read\t\t9999\talice\tread-only\ncount=1\n";
        let v = parse_standing_body(body);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].scope_kind, "method_prefix");
        assert_eq!(v[0].method_prefix.as_deref(), Some("tool.web_read"));
        assert_eq!(v[0].expires_at, 9999);
    }

    #[test]
    fn parse_budgeted_standing_returns_call_and_cost_bounds() {
        let body = "std-1\tpayments\t\tagent_category\t\t\t\t\t9999\talice\t2\t1\t20000\t10000\tone paid call\ncount=1\n";
        let v = parse_standing_body(body);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].max_calls, Some(2));
        assert_eq!(v[0].calls_used, 1);
        assert_eq!(v[0].max_cost_micros, Some(20_000));
        assert_eq!(v[0].cost_used_micros, 10_000);
        assert_eq!(v[0].note, "one paid call");
    }

    #[test]
    fn standing_create_detail_does_not_copy_operator_note() {
        let req = StandingCreateRequest {
            category: "payments".into(),
            expires_at: 9999,
            granted_by: Some("alice".into()),
            note: Some("sensitive operator reason".into()),
            path_glob: None,
            scope_kind: Some("task".into()),
            task_id: Some("0123456789abcdef0123456789abcdef".into()),
            session_id: None,
            method_prefix: None,
            workspace_path_glob: None,
            max_calls: Some(2),
            max_cost_micros: Some(20_000),
            run_id: Some("run-1".into()),
        };
        let detail = standing_create_detail("agt-1", &req, req.note.as_ref().unwrap().len());
        assert!(detail.contains("agent_id=agt-1"));
        assert!(detail.contains("category=payments"));
        assert!(detail.contains("scope_kind=task"));
        assert!(detail.contains("max_calls=2"));
        assert!(detail.contains("max_cost_micros=20000"));
        assert!(detail.contains("note_len=25"));
        assert!(!detail.contains("sensitive operator reason"));
    }

    #[test]
    fn standing_create_response_can_echo_scope_context() {
        let value = serde_json::to_value(StandingCreateResponse {
            standing_id: "std_1".into(),
            task_id: Some("0123456789abcdef0123456789abcdef".into()),
            run_id: Some("run-1".into()),
        })
        .unwrap();
        assert_eq!(value["standing_id"], "std_1");
        assert_eq!(value["task_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(value["run_id"], "run-1");
    }

    #[test]
    fn parse_bounded_standing_returns_call_limits() {
        let body = "std-1\tbrowser\t\tmethod_prefix\t\t\ttool.web_read\t\t9999\talice\t5\t2\tread-only\ncount=1\n";
        let v = parse_standing_body(body);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].max_calls, Some(5));
        assert_eq!(v[0].calls_used, 2);
        assert_eq!(v[0].note, "read-only");
    }

    #[test]
    fn create_agent_response_omits_token_when_none() {
        let resp = CreateAgentResponse {
            agent_id: "agt_x_123".into(),
            token: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"agent_id\":\"agt_x_123\""));
        assert!(
            !json.contains("token"),
            "token field must be absent when None"
        );
    }

    #[test]
    fn create_agent_response_includes_token_when_some() {
        let resp = CreateAgentResponse {
            agent_id: "agt_x_456".into(),
            token: Some("tok_abc".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"token\":\"tok_abc\""));
    }

    #[test]
    fn agent_token_response_serialises_agent_id_and_token() {
        let resp = AgentTokenResponse {
            agent_id: "agt_y_789".into(),
            token: "wire_token_xyz".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"agent_id\":\"agt_y_789\""));
        assert!(json.contains("\"token\":\"wire_token_xyz\""));
    }

    #[test]
    fn issue_agent_token_request_defaults_to_empty_scopes() {
        let req: IssueAgentTokenRequest = serde_json::from_str("{}").unwrap();
        assert!(req.scopes.is_empty());
        assert!(req.ttl_secs.is_none());
    }

    #[test]
    fn issue_agent_token_request_accepts_scopes_and_ttl() {
        let req: IssueAgentTokenRequest =
            serde_json::from_str(r#"{"scopes":["read","write"],"ttl_secs":3600}"#).unwrap();
        assert_eq!(req.scopes, vec!["read", "write"]);
        assert_eq!(req.ttl_secs, Some(3600));
    }
}
