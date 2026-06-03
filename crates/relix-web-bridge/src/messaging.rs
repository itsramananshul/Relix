//! HTTP proxies for agent-to-agent messaging.
//!
//! Five endpoints — all forward to the coordinator's `msg.*`
//! capabilities and reshape the tab-delimited wire body
//! into typed JSON.
//!
//! - `POST   /v1/messages                        ` — send.
//! - `GET    /v1/messages/inbox/:subject_id      ` — list inbox newest-first.
//! - `POST   /v1/messages/:message_id/read       ` — mark read.
//! - `GET    /v1/messages/thread/:thread_id      ` — full thread oldest-first.
//! - `DELETE /v1/messages/:message_id            ` — soft delete (status=expired).

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
use crate::tenant::{DEFAULT_TENANT, SubjectError, current_subject, current_tenant};

const DEFAULT_PEER: &str = "coordinator";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct MessageRow {
    pub message_id: String,
    pub thread_id: String,
    pub from_subject_id: String,
    pub subject: String,
    pub body_preview: String,
    pub sent_at: i64,
    pub read_at: Option<i64>,
    pub status: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct MessageListResponse {
    pub messages: Vec<MessageRow>,
    pub count: usize,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ThreadResponse {
    pub thread_id: String,
    pub messages: Vec<MessageRow>,
}

#[derive(Debug, Deserialize, Default)]
pub struct SendRequest {
    #[serde(default)]
    pub from_subject_id: Option<String>,
    #[serde(default)]
    pub to_subject_id: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub reply_to_message_id: Option<String>,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
    #[serde(default)]
    pub origin_surface: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SendResponse {
    pub message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct InboxQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub include_read: Option<u8>,
    #[serde(default)]
    pub since_message_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ReadRequest {
    #[serde(default)]
    pub reader_subject_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct DeleteRequest {
    #[serde(default)]
    pub subject_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ThreadQuery {
    #[serde(default)]
    pub subject_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OkResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

// ── Handlers ─────────────────────────────────────────────

pub async fn send(
    State(state): State<AppState>,
    Json(req): Json<SendRequest>,
) -> Result<Json<SendResponse>, (StatusCode, Json<ApiError>)> {
    // GROUP 1 PHASE 1A: the sender is the AUTHENTICATED caller —
    // never the body's `from_subject_id`. A body value that
    // disagrees with the authenticated subject is a spoof
    // attempt → 403.
    let from = require_caller_subject(req.from_subject_id.as_deref())?;
    let to = require_field(&req.to_subject_id, "to_subject_id")?;
    let body = require_field(&req.body, "body")?;
    let subject = req.subject.unwrap_or_default();
    let thread_id = req.thread_id.unwrap_or_default();
    let reply_to = req.reply_to_message_id.unwrap_or_default();
    let origin = req
        .origin_surface
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("api")
        .to_string();
    let ttl = req.ttl_secs.unwrap_or(0);
    for (name, val) in [
        ("from_subject_id", from.as_str()),
        ("to_subject_id", to.as_str()),
        ("subject", subject.as_str()),
        ("thread_id", thread_id.as_str()),
        ("reply_to_message_id", reply_to.as_str()),
        ("origin_surface", origin.as_str()),
    ] {
        if val.contains('|') {
            return Err(bad(format!("{name} must not contain `|`")));
        }
    }
    // body is the only field allowed to contain `|`; the
    // coordinator's parser uses splitn(8, '|') so the body's
    // tail is absorbed by the last split slot — wait, body
    // is the 4th slot, not the last. Reject `|` in body too.
    if body.contains('|') {
        return Err(bad("body must not contain `|`".into()));
    }
    let task_id = clean_optional_id(req.task_id.as_deref(), "task_id")?;
    let run_id = clean_optional(req.run_id.as_deref());
    let arg = format!("{from}|{to}|{subject}|{body}|{thread_id}|{reply_to}|{ttl}|{origin}");
    let detail = send_detail(&to, subject.len(), body.len(), ttl, &origin);
    let body = match call_peer_string(
        &state,
        DEFAULT_PEER,
        "msg.send",
        arg.as_bytes(),
        task_id.as_deref(),
    )
    .await
    {
        Ok(body) => {
            record_message_activity(
                &state,
                task_id.as_deref(),
                run_id.as_deref(),
                "msg.send",
                "ok",
                &detail,
            );
            body
        }
        Err(err) => {
            record_message_activity(
                &state,
                task_id.as_deref(),
                run_id.as_deref(),
                "msg.send",
                "err",
                &detail,
            );
            return Err(err);
        }
    };
    Ok(Json(SendResponse {
        message_id: body.trim().to_string(),
        task_id,
        run_id,
    }))
}

pub async fn inbox(
    State(state): State<AppState>,
    Path(subject_id): Path<String>,
    Query(q): Query<InboxQuery>,
) -> Result<Json<MessageListResponse>, (StatusCode, Json<ApiError>)> {
    // GROUP 1 PHASE 1A: a caller may only read their OWN inbox.
    // The subject is the authenticated caller; the path segment
    // may only agree with it (or it's a spoof attempt → 403).
    let subject_id = require_caller_subject(Some(&subject_id))?;
    let limit = q.limit.unwrap_or(20);
    let include_read = q.include_read.unwrap_or(0);
    let since = q.since_message_id.unwrap_or_default();
    let arg = format!("{subject_id}|{limit}|{include_read}|{since}");
    let body = call_peer_string(&state, DEFAULT_PEER, "msg.inbox", arg.as_bytes(), None).await?;
    let messages = parse_rows(&body);
    let count = messages.len();
    Ok(Json(MessageListResponse { messages, count }))
}

pub async fn read(
    State(state): State<AppState>,
    Path(message_id): Path<String>,
    Json(req): Json<ReadRequest>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    // GROUP 1 PHASE 1A: the reader is the AUTHENTICATED caller —
    // a caller may only mark their OWN messages read.
    let reader = require_caller_subject(req.reader_subject_id.as_deref())?;
    if reader.contains('|') || message_id.contains('|') {
        return Err(bad("ids must not contain `|`".into()));
    }
    let task_id = clean_optional_id(req.task_id.as_deref(), "task_id")?;
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = message_state_detail(&message_id);
    let arg = format!("{message_id}|{reader}");
    match call_peer_string(
        &state,
        DEFAULT_PEER,
        "msg.read",
        arg.as_bytes(),
        task_id.as_deref(),
    )
    .await
    {
        Ok(_) => {
            record_message_activity(
                &state,
                task_id.as_deref(),
                run_id.as_deref(),
                "msg.read",
                "ok",
                &detail,
            );
            Ok(Json(OkResponse {
                ok: true,
                task_id,
                run_id,
            }))
        }
        Err(err) => {
            record_message_activity(
                &state,
                task_id.as_deref(),
                run_id.as_deref(),
                "msg.read",
                "err",
                &detail,
            );
            Err(err)
        }
    }
}

pub async fn thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Query(q): Query<ThreadQuery>,
) -> Result<Json<ThreadResponse>, (StatusCode, Json<ApiError>)> {
    // GROUP 1 PHASE 1A: thread reads are scoped to the
    // AUTHENTICATED caller's subject, never a wire-supplied one.
    let subject = require_caller_subject(q.subject_id.as_deref())?;
    if subject.contains('|') || thread_id.contains('|') {
        return Err(bad("ids must not contain `|`".into()));
    }
    let arg = format!("{thread_id}|{subject}");
    let body = call_peer_string(&state, DEFAULT_PEER, "msg.thread", arg.as_bytes(), None).await?;
    let messages = parse_rows(&body);
    Ok(Json(ThreadResponse {
        thread_id,
        messages,
    }))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(message_id): Path<String>,
    Json(req): Json<DeleteRequest>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ApiError>)> {
    // GROUP 1 PHASE 1A: a caller may only delete their OWN
    // messages; the subject is the authenticated caller.
    let subject = require_caller_subject(req.subject_id.as_deref())?;
    if subject.contains('|') || message_id.contains('|') {
        return Err(bad("ids must not contain `|`".into()));
    }
    let task_id = clean_optional_id(req.task_id.as_deref(), "task_id")?;
    let run_id = clean_optional(req.run_id.as_deref());
    let detail = message_state_detail(&message_id);
    let arg = format!("{message_id}|{subject}");
    match call_peer_string(
        &state,
        DEFAULT_PEER,
        "msg.delete",
        arg.as_bytes(),
        task_id.as_deref(),
    )
    .await
    {
        Ok(_) => {
            record_message_activity(
                &state,
                task_id.as_deref(),
                run_id.as_deref(),
                "msg.delete",
                "ok",
                &detail,
            );
            Ok(Json(OkResponse {
                ok: true,
                task_id,
                run_id,
            }))
        }
        Err(err) => {
            record_message_activity(
                &state,
                task_id.as_deref(),
                run_id.as_deref(),
                "msg.delete",
                "err",
                &detail,
            );
            Err(err)
        }
    }
}

// ── Parsers ──────────────────────────────────────────────

pub fn parse_rows(body: &str) -> Vec<MessageRow> {
    body.lines()
        .filter(|line| !line.starts_with("count=") && !line.trim().is_empty())
        .filter_map(|line| {
            let cols: Vec<&str> = line.splitn(8, '\t').collect();
            if cols.len() != 8 {
                return None;
            }
            let read_at_raw: i64 = cols[6].parse().ok()?;
            Some(MessageRow {
                message_id: cols[0].into(),
                thread_id: cols[1].into(),
                from_subject_id: cols[2].into(),
                subject: cols[3].into(),
                body_preview: cols[4].into(),
                sent_at: cols[5].parse().ok()?,
                read_at: if read_at_raw < 0 {
                    None
                } else {
                    Some(read_at_raw)
                },
                status: cols[7].into(),
            })
        })
        .collect()
}

// ── Helpers ──────────────────────────────────────────────

fn require_field(v: &Option<String>, name: &str) -> Result<String, (StatusCode, Json<ApiError>)> {
    let s = v.as_deref().unwrap_or("").trim();
    if s.is_empty() {
        return Err(bad(format!("{name} is required")));
    }
    Ok(s.to_string())
}

/// GROUP 1 PHASE 1A: resolve the authenticated caller subject for
/// an identity-bound message operation, mapping the auth failure
/// to this module's HTTP error shape. Identity comes from the
/// authenticated principal channel ([`crate::tenant::current_subject`]),
/// never from the request body; a body/path claim may only agree
/// with it.
fn require_caller_subject(
    body_claim: Option<&str>,
) -> Result<String, (StatusCode, Json<ApiError>)> {
    crate::tenant::require_caller_subject(body_claim).map_err(subject_err)
}

fn subject_err(e: SubjectError) -> (StatusCode, Json<ApiError>) {
    match e {
        SubjectError::Unauthenticated => (
            StatusCode::UNAUTHORIZED,
            Json(ApiError {
                error: "caller subject not authenticated; the bridge derives identity \
                        from the authenticated X-Relix-Subject principal channel, not \
                        the request body"
                    .into(),
            }),
        ),
        SubjectError::Forbidden {
            claimed,
            authenticated,
        } => (
            StatusCode::FORBIDDEN,
            Json(ApiError {
                error: format!(
                    "subject `{claimed}` does not match the authenticated caller \
                     `{authenticated}`; a caller may only act as themselves"
                ),
            }),
        ),
    }
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

fn clean_optional_id(
    value: Option<&str>,
    field: &str,
) -> Result<Option<String>, (StatusCode, Json<ApiError>)> {
    let Some(clean) = clean_optional(value) else {
        return Ok(None);
    };
    if clean.len() == 32 && clean.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(Some(clean))
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!("{field} must be 32 hex chars"),
            }),
        ))
    }
}

fn send_detail(
    to_subject_id: &str,
    subject_len: usize,
    body_len: usize,
    ttl_secs: i64,
    origin_surface: &str,
) -> String {
    format!(
        "to_subject_id={to_subject_id}; subject_len={subject_len}; body_len={body_len}; ttl_secs={ttl_secs}; origin_surface={origin_surface}"
    )
}

fn message_state_detail(message_id: &str) -> String {
    format!("message_id={}", message_id.trim())
}

fn record_message_activity(
    state: &AppState,
    task_id: Option<&str>,
    run_id: Option<&str>,
    method: &str,
    decision: &str,
    detail: &str,
) {
    let tenant_id = current_tenant().unwrap_or_else(|| DEFAULT_TENANT.to_string());
    let actor = current_subject().unwrap_or_else(|| method.to_string());
    if let Err(e) = append_tool_invocation_activity(
        state.cfg.transport.data_dir.as_deref(),
        ToolInvocationActivity {
            tenant_id: &tenant_id,
            actor: &actor,
            peer: DEFAULT_PEER,
            method,
            task_id,
            run_id,
            decision,
            detail,
        },
    ) {
        tracing::warn!(error = %e, method, "failed to append messaging activity");
    }
    if let (Some(rec), Some(task_id)) = (state.task_recorder.as_ref(), task_id) {
        let payload = format!("peer={DEFAULT_PEER} outcome={decision} {detail}");
        let rec = rec.clone();
        let task_id = task_id.to_string();
        let event_type = method.to_string();
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
    fn parse_two_row_body_with_count_line() {
        let body = "m1\tt1\talice\thi\thello world\t100\t-1\tdelivered\n\
                    m2\tt1\tbob\tre\they\t200\t250\tread\n\
                    count=2\n";
        let v = parse_rows(body);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].message_id, "m1");
        assert!(v[0].read_at.is_none());
        assert_eq!(v[1].read_at, Some(250));
        assert_eq!(v[1].status, "read");
    }

    #[test]
    fn parse_empty_body_returns_empty_vec() {
        assert!(parse_rows("").is_empty());
        assert!(parse_rows("count=0\n").is_empty());
    }

    #[test]
    fn parse_skips_rows_with_wrong_column_count() {
        let body = "too\tfew\tcolumns\nm1\tt1\talice\thi\thello\t100\t-1\tdelivered\ncount=1\n";
        let v = parse_rows(body);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].message_id, "m1");
    }

    #[test]
    fn send_request_accepts_task_and_run_context() {
        let req: SendRequest = serde_json::from_value(serde_json::json!({
            "to_subject_id": "agent-b",
            "subject": "handoff",
            "body": "details",
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
    fn read_and_delete_requests_accept_task_and_run_context() {
        let read: ReadRequest = serde_json::from_value(serde_json::json!({
            "reader_subject_id": "agent-a",
            "task_id": "0123456789abcdef0123456789abcdef",
            "run_id": "run-1"
        }))
        .unwrap();
        assert_eq!(
            read.task_id.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(read.run_id.as_deref(), Some("run-1"));

        let delete: DeleteRequest = serde_json::from_value(serde_json::json!({
            "subject_id": "agent-a",
            "task_id": "fedcba9876543210fedcba9876543210",
            "run_id": "run-2"
        }))
        .unwrap();
        assert_eq!(
            delete.task_id.as_deref(),
            Some("fedcba9876543210fedcba9876543210")
        );
        assert_eq!(delete.run_id.as_deref(), Some("run-2"));
        assert_eq!(message_state_detail("m1"), "message_id=m1");
    }

    #[test]
    fn mutation_responses_omit_empty_scope_and_include_present_scope() {
        let bare = serde_json::to_value(SendResponse {
            message_id: "m1".into(),
            task_id: None,
            run_id: None,
        })
        .unwrap();
        assert!(bare.get("task_id").is_none());
        assert!(bare.get("run_id").is_none());

        let scoped = serde_json::to_value(SendResponse {
            message_id: "m2".into(),
            task_id: Some("0123456789abcdef0123456789abcdef".into()),
            run_id: Some("run-2".into()),
        })
        .unwrap();
        assert_eq!(scoped["task_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(scoped["run_id"], "run-2");

        let ok = serde_json::to_value(OkResponse {
            ok: true,
            task_id: Some("fedcba9876543210fedcba9876543210".into()),
            run_id: Some("run-3".into()),
        })
        .unwrap();
        assert_eq!(ok["task_id"], "fedcba9876543210fedcba9876543210");
        assert_eq!(ok["run_id"], "run-3");
    }

    #[test]
    fn clean_optional_id_rejects_invalid_task_id() {
        assert!(clean_optional_id(None, "task_id").unwrap().is_none());
        assert_eq!(
            clean_optional_id(Some(" 0123456789abcdef0123456789abcdef "), "task_id")
                .unwrap()
                .as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        let err = clean_optional_id(Some("not-a-task"), "task_id").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.0.error, "task_id must be 32 hex chars");
    }

    #[test]
    fn send_detail_records_lengths_without_message_content() {
        let detail = send_detail(
            "agent-b",
            "secret subject".len(),
            "secret body".len(),
            60,
            "api",
        );
        assert_eq!(
            detail,
            "to_subject_id=agent-b; subject_len=14; body_len=11; ttl_secs=60; origin_surface=api"
        );
        assert!(!detail.contains("secret subject"));
        assert!(!detail.contains("secret body"));
    }
}
