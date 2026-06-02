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
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

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
}

#[derive(Debug, Serialize)]
pub struct OkResponse {
    pub ok: bool,
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
    let token = try_issue_agent_token(
        &state,
        &agent_id,
        &detail.name,
        &req.scopes,
        req.ttl_secs,
    )
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

    if commits.is_empty() {
        return Err(bad("at least one updatable field required".into()));
    }
    for (field, value) in commits {
        let arg = format!("{agent_id}|{field}|{value}");
        let _ = call_peer_string(&state, DEFAULT_PEER, "agent.update", arg.as_bytes()).await?;
        applied = true;
    }
    let _ = applied;
    Ok(Json(OkResponse { ok: true }))
}

pub async fn delete_agent(
    State(state): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let _ = call_peer_string(&state, DEFAULT_PEER, "agent.delete", agent_id.as_bytes()).await?;
    Ok(Json(OkResponse { ok: true }))
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
    pub expires_at: i64,
    pub granted_by: String,
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
}

#[derive(Debug, Serialize)]
pub struct StandingCreateResponse {
    pub standing_id: String,
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
    let granted_by = req.granted_by.unwrap_or_else(|| "operator".to_string());
    let note = req.note.unwrap_or_default();
    let path_glob = req.path_glob.unwrap_or_default();
    let arg = format!(
        "{agent_id}|{}|{}|{granted_by}|{note}|{path_glob}",
        req.category, req.expires_at
    );
    let body = call_peer_string(
        &state,
        DEFAULT_PEER,
        "agent.standing_approval.create",
        arg.as_bytes(),
    )
    .await?;
    Ok(Json(StandingCreateResponse {
        standing_id: body.trim().to_string(),
    }))
}

pub async fn revoke_standing(
    State(state): State<AppState>,
    Path(standing_id): Path<String>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let _ = call_peer_string(
        &state,
        DEFAULT_PEER,
        "agent.standing_approval.revoke",
        standing_id.as_bytes(),
    )
    .await?;
    Ok(Json(OkResponse { ok: true }))
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
            let cols: Vec<&str> = line.splitn(6, '\t').collect();
            if cols.len() != 6 {
                return None;
            }
            Some(StandingRow {
                standing_id: cols[0].into(),
                match_category: cols[1].into(),
                match_path_glob: if cols[2].is_empty() {
                    None
                } else {
                    Some(cols[2].into())
                },
                expires_at: cols[3].parse().ok()?,
                granted_by: cols[4].into(),
                note: cols[5].into(),
            })
        })
        .collect()
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

async fn call_peer_string(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
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
    let resp = call_peer_json(state, DEFAULT_PEER, "identity.issue_token", &Value::Object(body))
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
        let body = "agent_id=id1|name=Alice|role=research|title=Junior|department=rd|team=ops|created_by=alice|status=active|subject_id=subj-1|risk_ceiling=medium|approval_timeout_secs=86400|created_at=100|updated_at=200|surface_allowlist=telegram,openwebui|allow_categories=browser,fetch|deny_categories=payments|allow_sensitivity_tags=|deny_sensitivity_tags=credentials:read|approval_required_categories=payments,production_deploy\n";
        let d = parse_agent_detail(body).unwrap();
        assert_eq!(d.agent_id, "id1");
        assert_eq!(d.allow_categories, vec!["browser", "fetch"]);
        assert_eq!(d.deny_sensitivity_tags, vec!["credentials:read"]);
        assert_eq!(d.surface_allowlist, vec!["telegram", "openwebui"]);
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
    fn create_agent_response_omits_token_when_none() {
        let resp = CreateAgentResponse {
            agent_id: "agt_x_123".into(),
            token: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"agent_id\":\"agt_x_123\""));
        assert!(!json.contains("token"), "token field must be absent when None");
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
