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

use crate::config::AppState;
use crate::tenant::SubjectError;

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
}

#[derive(Debug, Serialize)]
pub struct SendResponse {
    pub message_id: String,
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
}

#[derive(Debug, Deserialize, Default)]
pub struct DeleteRequest {
    #[serde(default)]
    pub subject_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ThreadQuery {
    #[serde(default)]
    pub subject_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OkResponse {
    pub ok: bool,
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
    let arg = format!("{from}|{to}|{subject}|{body}|{thread_id}|{reply_to}|{ttl}|{origin}");
    let body = call_peer_string(&state, DEFAULT_PEER, "msg.send", arg.as_bytes()).await?;
    Ok(Json(SendResponse {
        message_id: body.trim().to_string(),
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
    let body = call_peer_string(&state, DEFAULT_PEER, "msg.inbox", arg.as_bytes()).await?;
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
    let arg = format!("{message_id}|{reader}");
    let _ = call_peer_string(&state, DEFAULT_PEER, "msg.read", arg.as_bytes()).await?;
    Ok(Json(OkResponse { ok: true }))
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
    let body = call_peer_string(&state, DEFAULT_PEER, "msg.thread", arg.as_bytes()).await?;
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
    let arg = format!("{message_id}|{subject}");
    let _ = call_peer_string(&state, DEFAULT_PEER, "msg.delete", arg.as_bytes()).await?;
    Ok(Json(OkResponse { ok: true }))
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
}
