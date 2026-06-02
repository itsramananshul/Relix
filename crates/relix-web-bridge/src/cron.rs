//! HTTP proxies for the cron scheduler.
//!
//! Six endpoints — all of them proxy a single `cron.*`
//! capability on the coordinator peer and reshape the
//! pipe/tab-delimited wire body into typed JSON for the
//! dashboard.
//!
//! - `GET    /v1/cron/jobs?subject_id=<id>` → list jobs.
//! - `POST   /v1/cron/jobs` { name, schedule, flow_template,
//!   prompt, subject_id } → create.
//! - `GET    /v1/cron/jobs/:job_id` → one job.
//! - `PATCH  /v1/cron/jobs/:job_id` { enabled?, schedule?,
//!   prompt? } → update one or more fields (one underlying
//!   `cron.update` call per supplied field, applied in order).
//! - `DELETE /v1/cron/jobs/:job_id` → delete.
//! - `POST   /v1/cron/jobs/:job_id/trigger` → fire immediately.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::activity::{ToolInvocationActivity, append_tool_invocation_activity};
use crate::config::AppState;
use crate::tenant::{DEFAULT_TENANT, current_subject};

const DEFAULT_PEER: &str = "coordinator";

// ── Shared types ─────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

/// Lightweight job shape returned by `GET /v1/cron/jobs`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CronJobRow {
    pub job_id: String,
    pub name: String,
    pub schedule: String,
    pub next_run_at: i64,
    pub last_run_at: Option<i64>,
    pub enabled: bool,
    pub run_count: i64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CronListResponse {
    pub jobs: Vec<CronJobRow>,
    pub count: usize,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CronJobDetail {
    pub job_id: String,
    pub name: String,
    pub schedule: String,
    pub flow_template: String,
    pub prompt: String,
    pub subject_id: String,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_run_at: Option<i64>,
    pub next_run_at: i64,
    pub run_count: i64,
    pub last_task_id: Option<String>,
    pub last_status: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    pub subject_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct CreateRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub flow_template: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub subject_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateResponse {
    pub job_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ScopeQuery {
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OkResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TriggerResponse {
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

// ── Handlers ─────────────────────────────────────────────

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<CronListResponse>, (StatusCode, Json<ApiError>)> {
    let subject = q.subject_id.unwrap_or_default();
    let body =
        call_peer_string(&state, DEFAULT_PEER, "cron.list", subject.as_bytes(), None).await?;
    let jobs = parse_list_body(&body);
    let count = jobs.len();
    Ok(Json(CronListResponse { jobs, count }))
}

pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateRequest>,
) -> Result<Json<CreateResponse>, (StatusCode, Json<ApiError>)> {
    let task_id = clean_optional_task_id(req.task_id.as_deref(), "task_id")?;
    let run_id = clean_optional(req.run_id.as_deref());
    let name = require_field(&req.name, "name")?;
    let schedule = require_field(&req.schedule, "schedule")?;
    let flow_template = req
        .flow_template
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("flows/chat_template.sol")
        .to_string();
    let prompt = req.prompt.unwrap_or_default();
    let subject_id = require_field(&req.subject_id, "subject_id")?;
    let detail = create_detail(&name, &schedule, &flow_template, prompt.len(), &subject_id);
    // Reject pipes inside any field — they'd break the wire
    // format. Render a stable error rather than letting the
    // coordinator misparse.
    for (field, val) in [
        ("name", name.as_str()),
        ("schedule", schedule.as_str()),
        ("flow_template", flow_template.as_str()),
        ("prompt", prompt.as_str()),
        ("subject_id", subject_id.as_str()),
    ] {
        if field != "prompt" && val.contains('|') {
            return Err(bad(format!("{field} must not contain `|`")));
        }
    }
    // Prompt is the last field — it can contain `|` because
    // the coordinator's parser uses splitn(5, '|') and absorbs
    // the rest.
    let arg = format!("{name}|{schedule}|{flow_template}|{prompt}|{subject_id}");
    let body = match call_peer_string(
        &state,
        DEFAULT_PEER,
        "cron.create",
        arg.as_bytes(),
        task_id.as_deref(),
    )
    .await
    {
        Ok(body) => {
            record_cron_activity(
                &state,
                CronActivity {
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "cron.create",
                    decision: "ok",
                    detail: &detail,
                },
            );
            body
        }
        Err(err) => {
            record_cron_activity(
                &state,
                CronActivity {
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "cron.create",
                    decision: "err",
                    detail: &detail,
                },
            );
            return Err(err);
        }
    };
    let job_id = body.trim().to_string();
    Ok(Json(CreateResponse {
        job_id,
        task_id,
        run_id,
    }))
}

pub async fn get_one(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<CronJobDetail>, (StatusCode, Json<ApiError>)> {
    let body = call_peer_string(&state, DEFAULT_PEER, "cron.get", job_id.as_bytes(), None).await?;
    let parsed = parse_job_body(&body).ok_or((
        StatusCode::BAD_GATEWAY,
        Json(ApiError {
            error: format!("cron.get returned an unparseable body: {body:?}"),
        }),
    ))?;
    Ok(Json(parsed))
}

pub async fn update(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    Json(req): Json<UpdateRequest>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let task_id = clean_optional_task_id(req.task_id.as_deref(), "task_id")?;
    let run_id = clean_optional(req.run_id.as_deref());
    if req.enabled.is_none() && req.schedule.is_none() && req.prompt.is_none() {
        return Err(bad(
            "at least one of `enabled`, `schedule`, `prompt` is required".into(),
        ));
    }
    let detail = update_detail(&job_id, &req);
    // One coordinator round-trip per provided field; the
    // coordinator's cron.update only accepts one field at a
    // time. Order: enabled → schedule → prompt.
    if let Some(e) = req.enabled {
        let v = if e { "1" } else { "0" };
        let arg = format!("{job_id}|enabled|{v}");
        if let Err(err) = call_peer_string(
            &state,
            DEFAULT_PEER,
            "cron.update",
            arg.as_bytes(),
            task_id.as_deref(),
        )
        .await
        {
            record_cron_activity(
                &state,
                CronActivity {
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "cron.update",
                    decision: "err",
                    detail: &detail,
                },
            );
            return Err(err);
        }
    }
    if let Some(s) = req.schedule {
        if s.contains('|') {
            return Err(bad("schedule must not contain `|`".into()));
        }
        let arg = format!("{job_id}|schedule|{s}");
        if let Err(err) = call_peer_string(
            &state,
            DEFAULT_PEER,
            "cron.update",
            arg.as_bytes(),
            task_id.as_deref(),
        )
        .await
        {
            record_cron_activity(
                &state,
                CronActivity {
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "cron.update",
                    decision: "err",
                    detail: &detail,
                },
            );
            return Err(err);
        }
    }
    if let Some(p) = req.prompt {
        let arg = format!("{job_id}|prompt|{p}");
        if let Err(err) = call_peer_string(
            &state,
            DEFAULT_PEER,
            "cron.update",
            arg.as_bytes(),
            task_id.as_deref(),
        )
        .await
        {
            record_cron_activity(
                &state,
                CronActivity {
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "cron.update",
                    decision: "err",
                    detail: &detail,
                },
            );
            return Err(err);
        }
    }
    record_cron_activity(
        &state,
        CronActivity {
            task_id: task_id.as_deref(),
            run_id: run_id.as_deref(),
            method: "cron.update",
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

pub async fn delete(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    Query(q): Query<ScopeQuery>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let task_id = clean_optional_task_id(q.task_id.as_deref(), "task_id")?;
    let run_id = clean_optional(q.run_id.as_deref());
    let detail = job_detail("cron.delete", &job_id);
    match call_peer_string(
        &state,
        DEFAULT_PEER,
        "cron.delete",
        job_id.as_bytes(),
        task_id.as_deref(),
    )
    .await
    {
        Ok(_) => {
            record_cron_activity(
                &state,
                CronActivity {
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "cron.delete",
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
            record_cron_activity(
                &state,
                CronActivity {
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "cron.delete",
                    decision: "err",
                    detail: &detail,
                },
            );
            Err(err)
        }
    }
}

pub async fn trigger(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    Query(q): Query<ScopeQuery>,
) -> Result<Json<TriggerResponse>, (StatusCode, Json<ApiError>)> {
    let scope_task_id = clean_optional_task_id(q.task_id.as_deref(), "task_id")?;
    let run_id = clean_optional(q.run_id.as_deref());
    let detail = job_detail("cron.trigger", &job_id);
    let body = match call_peer_string(
        &state,
        DEFAULT_PEER,
        "cron.trigger",
        job_id.as_bytes(),
        scope_task_id.as_deref(),
    )
    .await
    {
        Ok(body) => body,
        Err(err) => {
            record_cron_activity(
                &state,
                CronActivity {
                    task_id: scope_task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "cron.trigger",
                    decision: "err",
                    detail: &detail,
                },
            );
            return Err(err);
        }
    };
    let task_id = body.trim().to_string();
    let activity_task_id = if is_valid_task_id(&task_id) {
        Some(task_id.as_str())
    } else {
        scope_task_id.as_deref()
    };
    record_cron_activity(
        &state,
        CronActivity {
            task_id: activity_task_id,
            run_id: run_id.as_deref(),
            method: "cron.trigger",
            decision: "ok",
            detail: &detail,
        },
    );
    Ok(Json(TriggerResponse {
        task_id,
        scope_task_id,
        run_id,
    }))
}

// ── Parsers ──────────────────────────────────────────────

pub fn parse_list_body(body: &str) -> Vec<CronJobRow> {
    body.lines()
        .filter(|line| !line.starts_with("count=") && !line.trim().is_empty())
        .filter_map(|line| {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() != 7 {
                return None;
            }
            let last_run_at: i64 = cols[4].parse().ok()?;
            Some(CronJobRow {
                job_id: cols[0].into(),
                name: cols[1].into(),
                schedule: cols[2].into(),
                next_run_at: cols[3].parse().ok()?,
                last_run_at: if last_run_at < 0 {
                    None
                } else {
                    Some(last_run_at)
                },
                enabled: cols[5] == "1",
                run_count: cols[6].parse().ok()?,
            })
        })
        .collect()
}

pub fn parse_job_body(body: &str) -> Option<CronJobDetail> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut job_id = String::new();
    let mut name = String::new();
    let mut schedule = String::new();
    let mut flow_template = String::new();
    let mut prompt = String::new();
    let mut subject_id = String::new();
    let mut enabled = true;
    let mut created_at: i64 = 0;
    let mut updated_at: i64 = 0;
    let mut last_run_at: Option<i64> = None;
    let mut next_run_at: i64 = 0;
    let mut run_count: i64 = 0;
    let mut last_task_id: Option<String> = None;
    let mut last_status: Option<String> = None;
    for kv in trimmed.split('|') {
        let (k, v) = kv.split_once('=')?;
        match k.trim() {
            "job_id" => job_id = v.into(),
            "name" => name = v.into(),
            "schedule" => schedule = v.into(),
            "flow_template" => flow_template = v.into(),
            "prompt" => prompt = v.into(),
            "subject_id" => subject_id = v.into(),
            "enabled" => enabled = v.trim() == "1",
            "created_at" => created_at = v.trim().parse().ok()?,
            "updated_at" => updated_at = v.trim().parse().ok()?,
            "last_run_at" => {
                let n: i64 = v.trim().parse().ok()?;
                last_run_at = if n < 0 { None } else { Some(n) };
            }
            "next_run_at" => next_run_at = v.trim().parse().ok()?,
            "run_count" => run_count = v.trim().parse().ok()?,
            "last_task_id" => {
                last_task_id = if v.is_empty() { None } else { Some(v.into()) };
            }
            "last_status" => {
                last_status = if v.is_empty() { None } else { Some(v.into()) };
            }
            _ => {}
        }
    }
    Some(CronJobDetail {
        job_id,
        name,
        schedule,
        flow_template,
        prompt,
        subject_id,
        enabled,
        created_at,
        updated_at,
        last_run_at,
        next_run_at,
        run_count,
        last_task_id,
        last_status,
    })
}

// ── Helpers ──────────────────────────────────────────────

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

fn clean_optional_task_id(
    value: Option<&str>,
    field: &str,
) -> Result<Option<String>, (StatusCode, Json<ApiError>)> {
    let Some(clean) = clean_optional(value) else {
        return Ok(None);
    };
    if is_valid_task_id(&clean) {
        Ok(Some(clean))
    } else {
        Err(bad(format!("{field} must be 32 hex chars")))
    }
}

fn is_valid_task_id(s: &str) -> bool {
    s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn create_detail(
    name: &str,
    schedule: &str,
    flow_template: &str,
    prompt_len: usize,
    subject_id: &str,
) -> String {
    format!(
        "method=cron.create; name={name}; schedule={schedule}; flow_template={flow_template}; prompt_len={prompt_len}; subject_id={subject_id}"
    )
}

fn update_detail(job_id: &str, req: &UpdateRequest) -> String {
    let fields = [
        req.enabled.map(|_| "enabled"),
        req.schedule.as_ref().map(|_| "schedule"),
        req.prompt.as_ref().map(|_| "prompt"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(",");
    let prompt_len = req.prompt.as_ref().map(|p| p.len()).unwrap_or(0);
    format!("method=cron.update; job_id={job_id}; fields={fields}; prompt_len={prompt_len}")
}

fn job_detail(method: &str, job_id: &str) -> String {
    format!("method={method}; job_id={job_id}")
}

struct CronActivity<'a> {
    task_id: Option<&'a str>,
    run_id: Option<&'a str>,
    method: &'a str,
    decision: &'a str,
    detail: &'a str,
}

fn record_cron_activity(state: &AppState, activity: CronActivity<'_>) {
    let tenant_id = crate::tenant::current_tenant_or_none()
        .as_deref()
        .unwrap_or(DEFAULT_TENANT)
        .to_string();
    let actor = current_subject().unwrap_or_else(|| activity.method.to_string());
    if let Err(e) = append_tool_invocation_activity(
        state.cfg.transport.data_dir.as_deref(),
        ToolInvocationActivity {
            tenant_id: &tenant_id,
            actor: &actor,
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
            "failed to append cron activity"
        );
    }
    if let (Some(rec), Some(task_id)) = (state.task_recorder.as_ref(), activity.task_id) {
        let payload = format!(
            "peer={DEFAULT_PEER} outcome={} {}",
            activity.decision, activity.detail
        );
        let rec = rec.clone();
        let task_id = task_id.to_string();
        let event_type = activity.method.to_string();
        tokio::spawn(async move {
            rec.event(&task_id, &event_type, &payload).await;
        });
    }
}

async fn call_peer_string(
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
            // Map INVALID_ARGS (== "not found" for unknown ids)
            // to 404, otherwise 502.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list_two_rows_then_count_line() {
        let body = "abc\tdaily\t1d\t100\t-1\t1\t0\nxyz\tweekly\t7d\t200\t150\t0\t2\ncount=2\n";
        let v = parse_list_body(body);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].job_id, "abc");
        assert_eq!(v[0].name, "daily");
        assert!(v[0].enabled);
        assert_eq!(v[0].last_run_at, None);
        assert!(!v[1].enabled);
        assert_eq!(v[1].last_run_at, Some(150));
    }

    #[test]
    fn parse_list_empty_body_returns_empty_vec() {
        assert!(parse_list_body("").is_empty());
        // Just the count line — no rows.
        assert!(parse_list_body("count=0\n").is_empty());
    }

    #[test]
    fn parse_job_body_round_trips_every_field() {
        let body = "job_id=abc|name=daily|schedule=1d|flow_template=f.sol|prompt=summarise|subject_id=subj|enabled=1|created_at=100|updated_at=200|last_run_at=-1|next_run_at=86500|run_count=0|last_task_id=|last_status=\n";
        let j = parse_job_body(body).unwrap();
        assert_eq!(j.job_id, "abc");
        assert_eq!(j.name, "daily");
        assert_eq!(j.schedule, "1d");
        assert_eq!(j.flow_template, "f.sol");
        assert_eq!(j.prompt, "summarise");
        assert!(j.enabled);
        assert_eq!(j.created_at, 100);
        assert_eq!(j.updated_at, 200);
        assert_eq!(j.last_run_at, None);
        assert_eq!(j.next_run_at, 86500);
        assert_eq!(j.run_count, 0);
        assert!(j.last_task_id.is_none());
        assert!(j.last_status.is_none());
    }

    #[test]
    fn parse_job_body_after_a_run_returns_last_task_id_and_status() {
        let body = "job_id=abc|name=daily|schedule=1d|flow_template=f.sol|prompt=p|subject_id=subj|enabled=1|created_at=100|updated_at=300|last_run_at=250|next_run_at=86500|run_count=1|last_task_id=task-1|last_status=ok\n";
        let j = parse_job_body(body).unwrap();
        assert_eq!(j.last_run_at, Some(250));
        assert_eq!(j.run_count, 1);
        assert_eq!(j.last_task_id.as_deref(), Some("task-1"));
        assert_eq!(j.last_status.as_deref(), Some("ok"));
    }

    #[test]
    fn parse_job_body_empty_is_none() {
        assert!(parse_job_body("").is_none());
    }

    #[test]
    fn create_request_accepts_task_and_run_context() {
        let req: CreateRequest = serde_json::from_value(serde_json::json!({
            "name": "daily",
            "schedule": "1d",
            "flow_template": "flows/chat_template.sol",
            "prompt": "summarize privately",
            "subject_id": "agent-1",
            "task_id": "0123456789abcdef0123456789abcdef",
            "run_id": "run-1"
        }))
        .unwrap();
        assert_eq!(
            req.task_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(req.run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn update_request_accepts_task_and_run_context() {
        let req: UpdateRequest = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "prompt": "new private prompt",
            "task_id": "0123456789abcdef0123456789abcdef",
            "run_id": "run-1"
        }))
        .unwrap();
        assert_eq!(
            req.task_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(req.run_id.as_deref(), Some("run-1"));
    }

    #[test]
    fn clean_optional_task_id_accepts_only_32_hex() {
        assert!(clean_optional_task_id(None, "task_id").unwrap().is_none());
        assert_eq!(
            clean_optional_task_id(Some(" 0123456789abcdef0123456789abcdef "), "task_id")
                .unwrap()
                .as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        let err = clean_optional_task_id(Some("task-1"), "task_id").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.0.error, "task_id must be 32 hex chars");
    }

    #[test]
    fn create_detail_does_not_copy_prompt() {
        let prompt = "private recurring instruction";
        let detail = create_detail(
            "daily",
            "1d",
            "flows/chat_template.sol",
            prompt.len(),
            "agent-1",
        );
        assert!(detail.contains("name=daily"));
        assert!(detail.contains("schedule=1d"));
        assert!(detail.contains("flow_template=flows/chat_template.sol"));
        assert!(detail.contains("prompt_len=29"));
        assert!(detail.contains("subject_id=agent-1"));
        assert!(!detail.contains(prompt));
    }

    #[test]
    fn update_detail_does_not_copy_prompt() {
        let prompt = "sensitive changed prompt";
        let req = UpdateRequest {
            enabled: Some(false),
            schedule: Some("2d".into()),
            prompt: Some(prompt.into()),
            task_id: None,
            run_id: None,
        };
        let detail = update_detail("job-1", &req);
        assert!(detail.contains("job_id=job-1"));
        assert!(detail.contains("fields=enabled,schedule,prompt"));
        assert!(detail.contains(&format!("prompt_len={}", prompt.len())));
        assert!(!detail.contains(prompt));
    }

    #[test]
    fn trigger_response_keeps_launched_task_and_scope_separate() {
        let value = serde_json::to_value(TriggerResponse {
            task_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            scope_task_id: Some("0123456789abcdef0123456789abcdef".into()),
            run_id: Some("run-1".into()),
        })
        .unwrap();
        assert_eq!(value["task_id"], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(value["scope_task_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(value["run_id"], "run-1");
    }
}
