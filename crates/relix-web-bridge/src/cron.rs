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

use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

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
}

#[derive(Debug, Serialize)]
pub struct CreateResponse {
    pub job_id: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OkResponse {
    pub ok: bool,
}

#[derive(Debug, Serialize)]
pub struct TriggerResponse {
    pub task_id: String,
}

// ── Handlers ─────────────────────────────────────────────

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<CronListResponse>, (StatusCode, Json<ApiError>)> {
    let subject = q.subject_id.unwrap_or_default();
    let body = call_peer_string(&state, DEFAULT_PEER, "cron.list", subject.as_bytes()).await?;
    let jobs = parse_list_body(&body);
    let count = jobs.len();
    Ok(Json(CronListResponse { jobs, count }))
}

pub async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateRequest>,
) -> Result<Json<CreateResponse>, (StatusCode, Json<ApiError>)> {
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
    let body = call_peer_string(&state, DEFAULT_PEER, "cron.create", arg.as_bytes()).await?;
    let job_id = body.trim().to_string();
    Ok(Json(CreateResponse { job_id }))
}

pub async fn get_one(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<CronJobDetail>, (StatusCode, Json<ApiError>)> {
    let body = call_peer_string(&state, DEFAULT_PEER, "cron.get", job_id.as_bytes()).await?;
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
    if req.enabled.is_none() && req.schedule.is_none() && req.prompt.is_none() {
        return Err(bad(
            "at least one of `enabled`, `schedule`, `prompt` is required".into(),
        ));
    }
    // One coordinator round-trip per provided field; the
    // coordinator's cron.update only accepts one field at a
    // time. Order: enabled → schedule → prompt.
    if let Some(e) = req.enabled {
        let v = if e { "1" } else { "0" };
        let arg = format!("{job_id}|enabled|{v}");
        let _ = call_peer_string(&state, DEFAULT_PEER, "cron.update", arg.as_bytes()).await?;
    }
    if let Some(s) = req.schedule {
        if s.contains('|') {
            return Err(bad("schedule must not contain `|`".into()));
        }
        let arg = format!("{job_id}|schedule|{s}");
        let _ = call_peer_string(&state, DEFAULT_PEER, "cron.update", arg.as_bytes()).await?;
    }
    if let Some(p) = req.prompt {
        let arg = format!("{job_id}|prompt|{p}");
        let _ = call_peer_string(&state, DEFAULT_PEER, "cron.update", arg.as_bytes()).await?;
    }
    Ok(Json(OkResponse { ok: true }))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    let _ = call_peer_string(&state, DEFAULT_PEER, "cron.delete", job_id.as_bytes()).await?;
    Ok(Json(OkResponse { ok: true }))
}

pub async fn trigger(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<TriggerResponse>, (StatusCode, Json<ApiError>)> {
    let body = call_peer_string(&state, DEFAULT_PEER, "cron.trigger", job_id.as_bytes()).await?;
    let task_id = body.trim().to_string();
    Ok(Json(TriggerResponse { task_id }))
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
    let envelope = build_request(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        deadline_secs,
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
}
