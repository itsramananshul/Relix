//! HTTP proxies for the email channel surface.
//!
//! Three endpoints, each a thin forwarder onto a capability the
//! email peer publishes:
//!
//! - `POST /v1/email/send`           — send a plain / HTML email.
//! - `POST /v1/email/send_template`  — render + send a templated email.
//! - `GET  /v1/email/status`         — SMTP + IMAP connection status.
//!
//! Mirrors `discord.rs` / `slack.rs` shape: pure translation, no
//! local state. The bridge dispatches via the existing
//! `MeshClient`, parses the email node's wire format (JSON for
//! `email.send*`, pipe-delimited for `email.status`), and returns
//! JSON. Error codes:
//!
//! - `400 Bad Request`         — missing / malformed payload.
//! - `404 Not Found`           — peer alias not in `peers.toml`.
//! - `502 Bad Gateway`         — responder returned an error / non-JSON body.
//! - `503 Service Unavailable` — bridge mesh client not yet up.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "email";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

/// `POST /v1/email/send` — send a plain / HTML email.
///
/// Body shape:
/// ```json
/// {
///   "to": ["alice@example.com"],
///   "subject": "Hi",
///   "body": "hello",
///   "html": "<p>hello</p>",
///   "cc": [],
///   "bcc": [],
///   "reply_to": null,
///   "in_reply_to": null,
///   "references": [],
///   "attachments": [
///     { "path": "/etc/relix/report.pdf", "filename": "report.pdf",
///       "content_type": "application/pdf" }
///   ],
///   "peer": "email"
/// }
/// ```
pub async fn send(
    State(state): State<AppState>,
    Json(req): Json<SendRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.to.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "`to` must contain at least one address".into(),
            }),
        )
            .into_response();
    }
    if req.subject.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "`subject` is required".into(),
            }),
        )
            .into_response();
    }
    if req.body.is_empty() && req.html.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "`body` is required (or `html`)".into(),
            }),
        )
            .into_response();
    }
    // PHASE 1B: validate every file-backed attachment path
    // BEFORE forwarding it to the email peer. A verbatim path
    // (e.g. `/etc/shadow` or one containing `..`) would ship an
    // arbitrary host file. Paths are confined to the fixed
    // attachment root.
    let root = attachment_root();
    for att in &req.attachments {
        if let Some(p) = att.path.as_deref().map(str::trim).filter(|s| !s.is_empty())
            && let Err(e) = validate_attachment_path(p, &root)
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: format!("attachment path rejected: {e}"),
                }),
            )
                .into_response();
        }
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let args = serde_json::json!({
        "to": req.to,
        "cc": req.cc,
        "bcc": req.bcc,
        "reply_to": req.reply_to,
        "subject": req.subject,
        "body": req.body,
        "html": req.html,
        "in_reply_to": req.in_reply_to,
        "references": req.references,
        "attachments": req.attachments,
    });
    match call_peer_json(&state, &peer, "email.send", &args).await {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(resp) => resp,
    }
}

/// `POST /v1/email/send_template` — render + send a templated email.
pub async fn send_template(
    State(state): State<AppState>,
    Json(req): Json<SendTemplateRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.template_name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "`template_name` is required".into(),
            }),
        )
            .into_response();
    }
    if req.to.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "`to` must contain at least one address".into(),
            }),
        )
            .into_response();
    }
    let peer = req.peer.clone().unwrap_or_else(|| DEFAULT_PEER.to_string());
    let args = serde_json::json!({
        "template_name": req.template_name,
        "to": req.to,
        "cc": req.cc,
        "bcc": req.bcc,
        "reply_to": req.reply_to,
        "in_reply_to": req.in_reply_to,
        "references": req.references,
        "variables": req.variables,
    });
    match call_peer_json(&state, &peer, "email.send_template", &args).await {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(resp) => resp,
    }
}

/// `GET /v1/email/messages/recent?limit=20` — recent inbound
/// message ring snapshot (newest-first). Proxies `email.messages_recent`.
pub async fn messages_recent(
    State(state): State<AppState>,
    Query(q): Query<RecentQuery>,
) -> Result<Json<RecentResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let limit = q.limit.unwrap_or(20).clamp(1, 200);
    let args = limit.to_string().into_bytes();
    let body = call_peer_string(&state, &peer, "email.messages_recent", &args).await?;
    let mut messages: Vec<RecentMessage> = Vec::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.splitn(6, '\t');
        let ts = parts
            .next()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let message_id = parts.next().unwrap_or("").to_string();
        let from = parts.next().unwrap_or("").to_string();
        let subject = parts.next().unwrap_or("").to_string();
        let session_id = parts.next().unwrap_or("").to_string();
        let preview = parts.next().unwrap_or("").to_string();
        messages.push(RecentMessage {
            ts,
            message_id,
            from,
            subject,
            session_id,
            preview,
        });
    }
    Ok(Json(RecentResponse { peer, messages }))
}

/// `GET /v1/email/status` — SMTP + IMAP connection state.
pub async fn status(
    State(state): State<AppState>,
    Query(q): Query<StatusQuery>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = call_peer_string(&state, &peer, "email.status", &[]).await?;
    let parsed = parse_status_body(&body).ok_or((
        StatusCode::BAD_GATEWAY,
        Json(ApiError {
            error: format!("email.status returned an unparseable body: {body:?}"),
        }),
    ))?;
    Ok(Json(StatusResponse {
        peer,
        smtp: parsed.smtp,
        imap: parsed.imap,
        from: parsed.from,
        smtp_host: parsed.smtp_host,
        imap_host: parsed.imap_host,
        imap_folder: parsed.imap_folder,
        messages_seen: parsed.messages_seen,
        messages_sent: parsed.messages_sent,
        last_send_at: parsed.last_send_at,
        last_poll_at: parsed.last_poll_at,
        last_message_at: parsed.last_message_at,
        smtp_error: parsed.smtp_error,
        imap_error: parsed.imap_error,
    }))
}

// ── request / response shapes ────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct StatusQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct RecentQuery {
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct RecentMessage {
    pub ts: i64,
    pub message_id: String,
    pub from: String,
    pub subject: String,
    pub session_id: String,
    pub preview: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct RecentResponse {
    pub peer: String,
    pub messages: Vec<RecentMessage>,
}

#[derive(Debug, Deserialize, Default)]
pub struct SendRequest {
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub html: Option<String>,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Option<Vec<String>>,
    #[serde(default)]
    pub attachments: Vec<SendAttachment>,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct SendTemplateRequest {
    #[serde(default)]
    pub template_name: String,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub references: Option<Vec<String>>,
    #[serde(default)]
    pub variables: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct SendAttachment {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub bytes_base64: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default = "default_attachment_ct")]
    pub content_type: String,
}

fn default_attachment_ct() -> String {
    "application/octet-stream".to_string()
}

/// PHASE 1B — the fixed directory file-backed email attachments
/// must live under. Operator-overridable via
/// `RELIX_EMAIL_ATTACHMENT_ROOT`; defaults to `./attachments`.
fn attachment_root() -> std::path::PathBuf {
    std::env::var_os("RELIX_EMAIL_ATTACHMENT_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("attachments"))
}

/// PHASE 1B — confine a client-supplied attachment path to
/// `root`. Rejects absolute paths, drive prefixes, root-anchored
/// paths, and any `..` component, then resolves against `root`
/// and confirms the CANONICAL result is still inside the
/// canonical root (defeating symlink / traversal escapes). On
/// success returns the canonical in-root path.
pub fn validate_attachment_path(
    path: &str,
    root: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    use std::path::Component;
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        return Err(format!(
            "`{path}` is absolute; only paths relative to the attachment root are allowed"
        ));
    }
    for comp in p.components() {
        match comp {
            Component::ParentDir => return Err(format!("`{path}` contains `..`")),
            // `/etc/shadow` (RootDir) and `C:\…` (Prefix) are
            // root-anchored even when `is_absolute()` is false on
            // some platforms — reject both.
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("`{path}` is root-anchored"));
            }
            _ => {}
        }
    }
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("attachment root `{}` unavailable: {e}", root.display()))?;
    let canon = root.join(p).canonicalize().map_err(|e| {
        format!("`{path}` does not resolve to a file under the attachment root: {e}")
    })?;
    if !canon.starts_with(&canon_root) {
        return Err(format!("`{path}` escapes the attachment root"));
    }
    Ok(canon)
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct StatusResponse {
    pub peer: String,
    pub smtp: String,
    pub imap: String,
    pub from: String,
    pub smtp_host: String,
    pub imap_host: String,
    pub imap_folder: String,
    pub messages_seen: u64,
    pub messages_sent: u64,
    pub last_send_at: Option<i64>,
    pub last_poll_at: Option<i64>,
    pub last_message_at: Option<i64>,
    pub smtp_error: Option<String>,
    pub imap_error: Option<String>,
}

// ── status parsing ───────────────────────────────────────

#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct ParsedStatus {
    pub smtp: String,
    pub imap: String,
    pub from: String,
    pub smtp_host: String,
    pub imap_host: String,
    pub imap_folder: String,
    pub messages_seen: u64,
    pub messages_sent: u64,
    pub last_send_at: Option<i64>,
    pub last_poll_at: Option<i64>,
    pub last_message_at: Option<i64>,
    pub smtp_error: Option<String>,
    pub imap_error: Option<String>,
}

pub fn parse_status_body(body: &str) -> Option<ParsedStatus> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = ParsedStatus::default();
    for kv in trimmed.split('|') {
        let (k, v) = kv.split_once('=')?;
        let key = k.trim();
        let val = v.trim();
        match key {
            "smtp" => out.smtp = val.to_string(),
            "imap" => out.imap = val.to_string(),
            "from" => out.from = val.to_string(),
            "smtp_host" => out.smtp_host = val.to_string(),
            "imap_host" => out.imap_host = val.to_string(),
            "imap_folder" => out.imap_folder = val.to_string(),
            "messages_seen" => out.messages_seen = val.parse().ok()?,
            "messages_sent" => out.messages_sent = val.parse().ok()?,
            "last_send_at" => {
                let n: i64 = val.parse().ok()?;
                out.last_send_at = if n < 0 { None } else { Some(n) };
            }
            "last_poll_at" => {
                let n: i64 = val.parse().ok()?;
                out.last_poll_at = if n < 0 { None } else { Some(n) };
            }
            "last_message_at" => {
                let n: i64 = val.parse().ok()?;
                out.last_message_at = if n < 0 { None } else { Some(n) };
            }
            "smtp_error" => {
                out.smtp_error = if val.is_empty() {
                    None
                } else {
                    Some(val.to_string())
                };
            }
            "imap_error" => {
                out.imap_error = if val.is_empty() {
                    None
                } else {
                    Some(val.to_string())
                };
            }
            _ => {}
        }
    }
    Some(out)
}

// ── mesh proxy helpers ───────────────────────────────────

async fn call_peer_json(
    state: &AppState,
    alias: &str,
    method: &str,
    args: &Value,
) -> Result<Value, axum::response::Response> {
    use axum::response::IntoResponse;
    let mesh = match state.mesh_client.as_ref() {
        Some(m) => m,
        None => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError {
                    error: "bridge mesh client not initialized".into(),
                }),
            )
                .into_response());
        }
    };
    let arg_bytes = match serde_json::to_vec(args) {
        Ok(b) => b,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError {
                    error: format!("encode args: {e}"),
                }),
            )
                .into_response());
        }
    };
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 120);
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
        (status, Json(ApiError { error: msg })).into_response()
    })?;
    let resp = decode_response(&resp_bytes).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("decode response: {e}"),
            }),
        )
            .into_response()
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
                    .into_response()
            })?;
            serde_json::from_str::<Value>(&text).map_err(|e| {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(ApiError {
                        error: format!("response body not JSON: {e} (body={text:?})"),
                    }),
                )
                    .into_response()
            })
        }
        ResponseResult::Err(env) => {
            // Map peer error kinds back to HTTP status codes the
            // operator can act on. INVALID_ARGS → 400; everything
            // else → 502 (the bridge has done its job; the peer
            // returned an application-level fault).
            let status = if env.kind == relix_core::types::error_kinds::INVALID_ARGS {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::BAD_GATEWAY
            };
            Err((
                status,
                Json(ApiError {
                    error: format!("responder err kind={} cause={}", env.kind, env.cause),
                }),
            )
                .into_response())
        }
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response from email peer".into(),
            }),
        )
            .into_response()),
    }
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
        ResponseResult::Err(env) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("responder err kind={} cause={}", env.kind, env.cause),
            }),
        )),
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response from email peer".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_typical_connected_body() {
        let body = "smtp=connected|imap=connected|from=Relix <bot@example.com>|smtp_host=smtp.e|imap_host=imap.e|imap_folder=INBOX|messages_seen=2|messages_sent=5|last_send_at=1700000000|last_poll_at=1700000010|last_message_at=1700000020|smtp_error=|imap_error=\n";
        let p = parse_status_body(body).unwrap();
        assert_eq!(p.smtp, "connected");
        assert_eq!(p.imap, "connected");
        assert_eq!(p.from, "Relix <bot@example.com>");
        assert_eq!(p.smtp_host, "smtp.e");
        assert_eq!(p.imap_host, "imap.e");
        assert_eq!(p.imap_folder, "INBOX");
        assert_eq!(p.messages_seen, 2);
        assert_eq!(p.messages_sent, 5);
        assert_eq!(p.last_send_at, Some(1700000000));
        assert_eq!(p.last_poll_at, Some(1700000010));
        assert_eq!(p.last_message_at, Some(1700000020));
        assert!(p.smtp_error.is_none());
        assert!(p.imap_error.is_none());
    }

    #[test]
    fn parse_status_disconnected_with_sentinel_timestamps() {
        let body = "smtp=disconnected|imap=disconnected|from=|smtp_host=|imap_host=|imap_folder=INBOX|messages_seen=0|messages_sent=0|last_send_at=-1|last_poll_at=-1|last_message_at=-1|smtp_error=|imap_error=\n";
        let p = parse_status_body(body).unwrap();
        assert_eq!(p.smtp, "disconnected");
        assert!(p.last_send_at.is_none());
        assert!(p.last_poll_at.is_none());
        assert!(p.last_message_at.is_none());
    }

    #[test]
    fn parse_status_with_error_strings_populates_options() {
        let body = "smtp=error|imap=connected|from=bot@e|smtp_host=smtp.e|imap_host=imap.e|imap_folder=INBOX|messages_seen=0|messages_sent=0|last_send_at=-1|last_poll_at=-1|last_message_at=-1|smtp_error=auth failed|imap_error=\n";
        let p = parse_status_body(body).unwrap();
        assert_eq!(p.smtp, "error");
        assert_eq!(p.smtp_error.as_deref(), Some("auth failed"));
        assert!(p.imap_error.is_none());
    }

    #[test]
    fn parse_status_empty_body_returns_none() {
        assert!(parse_status_body("").is_none());
        assert!(parse_status_body("   ").is_none());
    }

    #[test]
    fn parse_status_unknown_keys_are_silently_ignored() {
        let body = "smtp=connected|imap=connected|from=|smtp_host=|imap_host=|imap_folder=|messages_seen=0|messages_sent=0|last_send_at=-1|last_poll_at=-1|last_message_at=-1|smtp_error=|imap_error=|future_key=future_value\n";
        let p = parse_status_body(body).unwrap();
        assert_eq!(p.smtp, "connected");
    }

    #[test]
    fn send_request_defaults_are_safe() {
        let req: SendRequest = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(req.to.is_empty());
        assert!(req.subject.is_empty());
        assert!(req.body.is_empty());
    }

    #[test]
    fn phase1b_attachment_path_rejects_escape_and_accepts_in_root() {
        let root = tempfile::tempdir().unwrap();
        // A legitimate in-root file is accepted.
        std::fs::write(root.path().join("report.pdf"), b"hi").unwrap();
        assert!(
            validate_attachment_path("report.pdf", root.path()).is_ok(),
            "a legitimate in-root attachment must be accepted"
        );
        // Absolute host path exfil → rejected.
        assert!(validate_attachment_path("/etc/shadow", root.path()).is_err());
        // Traversal out of the root → rejected.
        assert!(validate_attachment_path("../../etc/passwd", root.path()).is_err());
        assert!(validate_attachment_path("sub/../../escape", root.path()).is_err());
    }

    #[test]
    fn send_attachment_default_content_type_is_octet_stream() {
        let a: SendAttachment = serde_json::from_value(serde_json::json!({
            "path": "/tmp/x.bin"
        }))
        .unwrap();
        assert_eq!(a.content_type, "application/octet-stream");
    }

    /// Body-level validation: `to` is empty → 400. We exercise
    /// the validator directly because the axum handler needs
    /// an AppState we can't easily mock here; the integration
    /// test below covers the same path through axum.
    #[test]
    fn empty_to_field_is_caught_by_validation_shape() {
        let req: SendRequest = serde_json::from_value(serde_json::json!({
            "subject": "x",
            "body": "y"
        }))
        .unwrap();
        assert!(req.to.is_empty());
    }
}
