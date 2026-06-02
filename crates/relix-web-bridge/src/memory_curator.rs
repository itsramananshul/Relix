//! W2-MEMORY-CURATOR-3 — HTTP proxies for the memory curator
//! surface.
//!
//! Two endpoints, both wired end-to-end to the memory node's
//! mesh capabilities (no placeholders, no "not yet readable"
//! sentinels):
//!
//! - `POST /v1/memory/curate` — operator-triggered curation
//!   for one subject_id. Proxies `memory.agent_curate` on the
//!   memory node and returns the parsed pipe-delimited
//!   summary as JSON.
//!
//! - `GET /v1/memory/curator/status` — proxies the memory
//!   node's `memory.curator_status` capability and projects
//!   the pipe-delimited body into the structured
//!   [`StatusResponse`]. Tells the operator whether the
//!   scheduler is enabled / configured, when it last ran,
//!   what it did, and when the next tick is due.
//!
//! Both endpoints return 502 when the memory peer responds
//! with an unparseable body and 503 when the bridge cannot
//! resolve the alias — honest about transport-side failures
//! rather than fabricating a synthesized reply.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::activity::{ToolInvocationActivity, append_tool_invocation_activity};
use crate::config::AppState;
use crate::tenant::{DEFAULT_TENANT, current_subject, current_tenant};

const DEFAULT_PEER: &str = "memory";
const DEFAULT_AI_PEER: &str = "ai";

/// POST `/v1/memory/curate` body. `subject_id` is required —
/// validated inside the handler so a missing field returns
/// `400` (with our `ApiError` JSON shape) rather than axum's
/// default 422 for JSON deserialization failures.
#[derive(Debug, Deserialize, Default)]
pub struct CurateRequest {
    /// The agent's 64-char hex subject_id.
    #[serde(default)]
    pub subject_id: Option<String>,
    /// Memory peer alias. Defaults to `"memory"`.
    #[serde(default)]
    pub peer: Option<String>,
    /// AI peer alias used by the memory node for this
    /// curation pass. Defaults to `"ai"`. The memory node
    /// configures the actual peer address; this alias is
    /// informational today (forward-compat for multi-AI
    /// routing).
    #[serde(default)]
    pub ai_peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

/// Parsed curation summary (one entry per field in the
/// memory-node wire body).
#[derive(Debug, Serialize, Default, Clone, PartialEq, Eq)]
pub struct CurateSummary {
    pub agent_entries_before: usize,
    pub agent_entries_after: usize,
    pub agent_chars_before: usize,
    pub agent_chars_after: usize,
    pub user_entries_before: usize,
    pub user_entries_after: usize,
    pub user_chars_before: usize,
    pub user_chars_after: usize,
    pub chars_saved: usize,
}

#[derive(Debug, Serialize)]
pub struct CurateResponse {
    pub peer: String,
    pub subject_id: String,
    pub result: CurateSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub async fn curate(
    State(state): State<AppState>,
    Json(req): Json<CurateRequest>,
) -> Result<Json<CurateResponse>, (StatusCode, Json<ApiError>)> {
    let subject_id = req.subject_id.as_deref().unwrap_or("").trim().to_string();
    if subject_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "subject_id required".into(),
            }),
        ));
    }
    let task_id = clean_optional_id(req.task_id.as_deref(), "task_id").map_err(bad)?;
    let run_id = clean_optional(req.run_id.as_deref());
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let ai_peer = req
        .ai_peer
        .clone()
        .unwrap_or_else(|| DEFAULT_AI_PEER.to_string());
    let detail = curate_detail(&subject_id, &ai_peer);
    let arg = format!("{subject_id}|{ai_peer}");
    let body = match call_peer_string(
        &state,
        &peer,
        "memory.agent_curate",
        arg.as_bytes(),
        task_id.as_deref(),
    )
    .await
    {
        Ok(body) => {
            record_curator_activity(
                &state,
                &peer,
                task_id.as_deref(),
                run_id.as_deref(),
                "ok",
                &detail,
            );
            body
        }
        Err(err) => {
            record_curator_activity(
                &state,
                &peer,
                task_id.as_deref(),
                run_id.as_deref(),
                "err",
                &detail,
            );
            return Err(err);
        }
    };
    let summary = parse_curate_body(&body).ok_or((
        StatusCode::BAD_GATEWAY,
        Json(ApiError {
            error: format!(
                "memory peer returned unparseable agent_curate body ({} chars)",
                body.len()
            ),
        }),
    ))?;
    Ok(Json(CurateResponse {
        peer,
        subject_id,
        result: summary,
        task_id,
        run_id,
    }))
}

#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Serialize, Default, Clone, PartialEq, Eq)]
pub struct StatusLastRun {
    pub agents_reviewed: usize,
    pub agents_curated: usize,
    pub total_chars_saved: usize,
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    /// Memory peer the bridge proxied the request to.
    pub peer: String,
    /// Whether the curator is configured + enabled.
    pub enabled: bool,
    /// Scheduler tick cadence (seconds).
    pub interval_secs: u64,
    /// Min combined chars before curation kicks in.
    pub min_chars_to_curate: usize,
    /// True while a scheduler tick is in progress.
    pub running: bool,
    /// Unix seconds of the last scheduler run, or `None`
    /// when no run has fired yet.
    pub last_run_at: Option<i64>,
    /// Unix seconds of the next scheduler tick.
    pub next_run_at: Option<i64>,
    /// Last-run telemetry. None when no run has fired.
    pub last_run_summary: Option<StatusLastRun>,
    /// True when [memory.curator] is configured on the
    /// memory node. When false, every other field is the
    /// "disabled" defaults; operators should add the config.
    pub configured: bool,
}

pub async fn status(
    State(state): State<AppState>,
    Query(q): Query<StatusQuery>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = call_peer_string(&state, &peer, "memory.curator_status", &[], None).await?;
    let parsed = parse_status_body(&body).ok_or((
        StatusCode::BAD_GATEWAY,
        Json(ApiError {
            error: format!(
                "memory peer returned unparseable curator_status body ({} chars)",
                body.len()
            ),
        }),
    ))?;
    Ok(Json(StatusResponse {
        peer,
        enabled: parsed.enabled,
        interval_secs: parsed.interval_secs,
        min_chars_to_curate: parsed.min_chars_to_curate,
        running: parsed.running,
        last_run_at: parsed.last_run_at,
        next_run_at: parsed.next_run_at,
        last_run_summary: parsed.last_run_summary,
        configured: parsed.configured,
    }))
}

/// Parse the pipe-delimited body emitted by the memory
/// node's `memory.curator_status` handler. The body is a
/// single line of `key=value|key=value|...` followed by a
/// trailing newline. Bad parses (missing keys, garbage
/// numbers) return `None`; the bridge surfaces that as 502.
///
/// `-1` is the sentinel for "no run yet" on the timestamp
/// fields; we map it back to `None` here so the JSON
/// response reads naturally.
pub fn parse_status_body(body: &str) -> Option<ParsedStatus> {
    let line = body.trim();
    if line.is_empty() {
        return None;
    }
    // Default: configured=true unless the body explicitly
    // sets configured=false (the disabled-default body the
    // memory node emits when [memory.curator] is missing).
    let mut p = ParsedStatus {
        configured: true,
        ..ParsedStatus::default()
    };
    let mut reviewed = 0usize;
    let mut curated = 0usize;
    let mut saved = 0usize;
    let mut saw_summary = false;
    for field in line.split('|') {
        let (k, v) = field.split_once('=')?;
        match k {
            "enabled" => p.enabled = v == "true",
            "interval_secs" => p.interval_secs = v.parse().ok()?,
            "min_chars_to_curate" => p.min_chars_to_curate = v.parse().ok()?,
            "running" => p.running = v == "true",
            "last_run_at" => {
                let n: i64 = v.parse().ok()?;
                p.last_run_at = if n < 0 { None } else { Some(n) };
            }
            "next_run_at" => {
                let n: i64 = v.parse().ok()?;
                p.next_run_at = if n < 0 { None } else { Some(n) };
            }
            "last_agents_reviewed" => {
                reviewed = v.parse().ok()?;
                saw_summary = true;
            }
            "last_agents_curated" => {
                curated = v.parse().ok()?;
                saw_summary = true;
            }
            "last_total_chars_saved" => {
                saved = v.parse().ok()?;
                saw_summary = true;
            }
            "configured" => p.configured = v == "true",
            _ => {} // forward-compat
        }
    }
    // Only surface a `last_run_summary` when the underlying
    // run actually happened (last_run_at present). Otherwise
    // we'd serve zeros as if they were a real "0 agents
    // reviewed" run, which is misleading.
    if saw_summary && p.last_run_at.is_some() {
        p.last_run_summary = Some(StatusLastRun {
            agents_reviewed: reviewed,
            agents_curated: curated,
            total_chars_saved: saved,
        });
    }
    Some(p)
}

/// Result shape from `parse_status_body`. Same fields as
/// `StatusResponse` minus `peer` (the bridge stamps it).
#[derive(Debug, Default)]
pub struct ParsedStatus {
    pub enabled: bool,
    pub interval_secs: u64,
    pub min_chars_to_curate: usize,
    pub running: bool,
    pub last_run_at: Option<i64>,
    pub next_run_at: Option<i64>,
    pub last_run_summary: Option<StatusLastRun>,
    pub configured: bool,
}

/// Parse the pipe-delimited body emitted by
/// `memory.agent_curate`. Returns `None` on any malformed
/// input. Tolerant of trailing whitespace.
pub fn parse_curate_body(body: &str) -> Option<CurateSummary> {
    let line = body.trim();
    if line.is_empty() {
        return None;
    }
    let mut out = CurateSummary::default();
    // Numeric fields require a usize parse; non-numeric fields
    // (subject_id, future model-name fields, etc.) pass through
    // untouched. Bad numeric value on a known field is treated
    // as a malformed body — the bridge prefers a clean 502 over
    // a silently-zeroed summary.
    for field in line.split('|') {
        let (k, v) = field.split_once('=')?;
        match k {
            "agent_entries_before" => out.agent_entries_before = v.parse().ok()?,
            "agent_entries_after" => out.agent_entries_after = v.parse().ok()?,
            "agent_chars_before" => out.agent_chars_before = v.parse().ok()?,
            "agent_chars_after" => out.agent_chars_after = v.parse().ok()?,
            "user_entries_before" => out.user_entries_before = v.parse().ok()?,
            "user_entries_after" => out.user_entries_after = v.parse().ok()?,
            "user_chars_before" => out.user_chars_before = v.parse().ok()?,
            "user_chars_after" => out.user_chars_after = v.parse().ok()?,
            "chars_saved" => out.chars_saved = v.parse().ok()?,
            _ => {} // subject_id, forward-compat fields ignored
        }
    }
    Some(out)
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
            error: "bridge mesh client not initialized (peer discovery failed at startup)".into(),
        }),
    ))?;
    // Bigger deadline than the default for curate — the
    // memory peer is calling out to the AI peer, which is
    // slow. Cap at 120s.
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(60, 120);
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
        ResponseResult::Err(env) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("responder err kind={} cause={}", env.kind, env.cause),
            }),
        )),
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response from memory.agent_curate".into(),
            }),
        )),
    }
}

fn bad(error: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError {
            error: error.into(),
        }),
    )
}

fn clean_optional(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn clean_optional_id(value: Option<&str>, field: &str) -> Result<Option<String>, String> {
    let Some(clean) = clean_optional(value) else {
        return Ok(None);
    };
    if clean.len() == 32 && clean.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(Some(clean))
    } else {
        Err(format!("{field} must be 32 hex chars"))
    }
}

fn curate_detail(subject_id: &str, ai_peer: &str) -> String {
    format!(
        "subject_id={}; ai_peer={}",
        subject_id.trim(),
        ai_peer.trim()
    )
}

fn record_curator_activity(
    state: &AppState,
    peer: &str,
    task_id: Option<&str>,
    run_id: Option<&str>,
    decision: &str,
    detail: &str,
) {
    let tenant_id = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
    let actor = current_subject().unwrap_or_else(|| "memory.curator".into());
    if let Err(e) = append_tool_invocation_activity(
        state.cfg.transport.data_dir.as_deref(),
        ToolInvocationActivity {
            tenant_id: &tenant_id,
            actor: &actor,
            peer,
            method: "memory.agent_curate",
            task_id,
            run_id,
            decision,
            detail,
        },
    ) {
        tracing::warn!(error = %e, "failed to append memory curator activity");
    }
    if let (Some(rec), Some(task_id)) = (state.task_recorder.as_ref(), task_id) {
        let payload = format!("peer={peer} outcome={decision} {detail}");
        let rec = rec.clone();
        let task_id = task_id.to_string();
        tokio::spawn(async move {
            rec.event(&task_id, "memory.agent_curate", &payload).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_typical_curate_body() {
        let body = "subject_id=alice|agent_entries_before=5|agent_entries_after=3|agent_chars_before=200|agent_chars_after=120|user_entries_before=3|user_entries_after=2|user_chars_before=80|user_chars_after=50|chars_saved=110\n";
        let s = parse_curate_body(body).unwrap();
        assert_eq!(s.agent_entries_before, 5);
        assert_eq!(s.agent_entries_after, 3);
        assert_eq!(s.agent_chars_before, 200);
        assert_eq!(s.agent_chars_after, 120);
        assert_eq!(s.user_entries_before, 3);
        assert_eq!(s.user_entries_after, 2);
        assert_eq!(s.user_chars_before, 80);
        assert_eq!(s.user_chars_after, 50);
        assert_eq!(s.chars_saved, 110);
    }

    #[test]
    fn parse_empty_body_returns_none() {
        assert!(parse_curate_body("").is_none());
        assert!(parse_curate_body("   ").is_none());
    }

    #[test]
    fn parse_rejects_malformed_field() {
        let body = "agent_chars_before=NaN|user_chars_after=10";
        assert!(parse_curate_body(body).is_none());
    }

    #[test]
    fn parse_status_typical_running_with_summary() {
        let body = "enabled=true|interval_secs=3600|min_chars_to_curate=100|running=false|last_run_at=1716000000|next_run_at=1716003600|last_agents_reviewed=5|last_agents_curated=3|last_total_chars_saved=120\n";
        let s = parse_status_body(body).unwrap();
        assert!(s.enabled);
        assert_eq!(s.interval_secs, 3600);
        assert_eq!(s.min_chars_to_curate, 100);
        assert!(!s.running);
        assert_eq!(s.last_run_at, Some(1716000000));
        assert_eq!(s.next_run_at, Some(1716003600));
        let summary = s.last_run_summary.expect("summary present");
        assert_eq!(summary.agents_reviewed, 5);
        assert_eq!(summary.agents_curated, 3);
        assert_eq!(summary.total_chars_saved, 120);
        assert!(s.configured);
    }

    #[test]
    fn parse_status_no_run_yet_drops_summary() {
        let body = "enabled=true|interval_secs=3600|min_chars_to_curate=100|running=false|last_run_at=-1|next_run_at=1716003600|last_agents_reviewed=0|last_agents_curated=0|last_total_chars_saved=0\n";
        let s = parse_status_body(body).unwrap();
        assert!(s.last_run_at.is_none());
        // No run yet → no summary surfaced (zeros are
        // ambiguous between "haven't run" and "ran, curated
        // nothing"; the timestamp disambiguates).
        assert!(s.last_run_summary.is_none());
    }

    #[test]
    fn parse_status_disabled_unconfigured_body() {
        let body = "enabled=false|interval_secs=0|min_chars_to_curate=0|running=false|last_run_at=-1|next_run_at=-1|last_agents_reviewed=0|last_agents_curated=0|last_total_chars_saved=0|configured=false\n";
        let s = parse_status_body(body).unwrap();
        assert!(!s.configured);
        assert!(!s.enabled);
    }

    #[test]
    fn parse_status_rejects_empty_body() {
        assert!(parse_status_body("").is_none());
        assert!(parse_status_body("   ").is_none());
    }

    #[test]
    fn parse_tolerates_extra_unknown_field_forward_compat() {
        // A future memory-node version that adds a `model=...`
        // field should still parse cleanly.
        let body = "subject_id=alice|agent_entries_before=1|agent_entries_after=1|agent_chars_before=10|agent_chars_after=10|user_entries_before=0|user_entries_after=0|user_chars_before=0|user_chars_after=0|chars_saved=0|model=gpt-99\n";
        let s = parse_curate_body(body).unwrap();
        assert_eq!(s.agent_chars_before, 10);
    }

    #[test]
    fn curate_request_accepts_task_and_run_context() {
        let req: CurateRequest = serde_json::from_str(
            r#"{
                "subject_id":"alice",
                "ai_peer":"ai-fast",
                "task_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "run_id":"run-1"
            }"#,
        )
        .unwrap();
        assert_eq!(
            req.task_id.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(req.run_id.as_deref(), Some("run-1"));
        assert_eq!(
            curate_detail(
                req.subject_id.as_deref().unwrap(),
                req.ai_peer.as_deref().unwrap()
            ),
            "subject_id=alice; ai_peer=ai-fast"
        );
    }

    #[test]
    fn clean_optional_id_rejects_invalid_task_id() {
        assert_eq!(
            clean_optional_id(Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "task_id")
                .unwrap()
                .as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert!(clean_optional_id(Some("bad"), "task_id").is_err());
        assert_eq!(clean_optional_id(Some(" "), "task_id").unwrap(), None);
    }

    #[test]
    fn curate_response_serialises_optional_scope() {
        let response = CurateResponse {
            peer: "memory".into(),
            subject_id: "alice".into(),
            result: CurateSummary::default(),
            task_id: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
            run_id: Some("run-1".into()),
        };
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["task_id"], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(value["run_id"], "run-1");
    }
}
