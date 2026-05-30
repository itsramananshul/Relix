//! PH-DASH-BROWSER — HTTP proxy for `tool.browser.list_sessions`.
//! Lets the dashboard show operators which browser sessions are
//! currently open on a tool node and which URL each one is on.
//!
//! One endpoint:
//!
//! - `GET /v1/browser/sessions?peer=<alias>` — proxies
//!   `tool.browser.list_sessions` to the named tool peer
//!   (default alias `"tool"`). Returns JSON:
//!
//!   ```json
//!   {
//!     "peer": "tool",
//!     "sessions": [
//!       {"session_id": "abc1234567890def",
//!        "opened_at": 1716000000,
//!        "current_url": "https://example.com/",
//!        "status": "connected"}
//!     ],
//!     "count": 1
//!   }
//!   ```
//!
//! Wire format from the responder is tab-delim:
//! `<session_id>\t<opened_at>\t<current_url>\t<status>\n`
//! with a trailing `count=N`. `current_url` arrives as `"-"`
//! when the session has not yet navigated; the bridge converts
//! `"-"` back to JSON `null` so the dashboard can render it as
//! a distinct empty state rather than the literal hyphen.
//!
//! Honest scope: read-only. Operators cannot open or close
//! sessions through this endpoint — for that they go through
//! `tool.browser.open_session` / `tool.browser.close_session`
//! via the existing dispatch path or a future
//! PH-BRIDGE-BROWSER-MUTATE proxy.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "tool";

#[derive(Debug, Deserialize)]
pub struct BrowserSessionsQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

/// PH-DASH-BROWSER: one row of the response. Mirrors what the
/// runtime's `tool.browser.list_sessions` wire format provides;
/// `page_title` is intentionally absent — the runtime's handler
/// doesn't expose it on the wire today. A future
/// PH-DASH-BROWSER-TITLE could either extend the wire format
/// or add a separate `tool.browser.info` capability and surface
/// page_title here.
#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct BrowserSessionRow {
    pub session_id: String,
    pub opened_at: i64,
    /// `None` when the session has not navigated yet. The wire
    /// format uses `"-"` as a sentinel; the bridge round-trips
    /// it to `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_url: Option<String>,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct BrowserSessionsResponse {
    pub peer: String,
    pub sessions: Vec<BrowserSessionRow>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub async fn sessions(
    State(state): State<AppState>,
    Query(q): Query<BrowserSessionsQuery>,
) -> Result<Json<BrowserSessionsResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER).to_string();
    let body = call_peer(&state, &peer, "tool.browser.list_sessions", b"").await?;
    let sessions = parse_sessions(&body);
    let count = sessions.len();
    Ok(Json(BrowserSessionsResponse {
        peer,
        sessions,
        count,
    }))
}

/// PH-DASH-BROWSER: parse the tab-delim body returned by
/// `tool.browser.list_sessions`:
///
/// ```text
/// <session_id>\t<opened_at>\t<current_url>\t<status>
/// ...
/// count=N
/// ```
///
/// `current_url == "-"` → JSON null.
fn parse_sessions(body: &str) -> Vec<BrowserSessionRow> {
    let mut out = Vec::new();
    for line in body.lines() {
        if line.starts_with("count=") || line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 4 {
            continue;
        }
        let opened_at = match parts[1].parse::<i64>() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let current_url = match parts[2] {
            "-" | "" => None,
            other => Some(other.to_string()),
        };
        out.push(BrowserSessionRow {
            session_id: parts[0].to_string(),
            opened_at,
            current_url,
            status: parts[3].to_string(),
        });
    }
    out
}

/// PH-DASH-BROWSER: dial-and-call the named peer. Same error
/// classification as `fs_audit::call_peer` / `term_audit::call_peer`
/// / `blocklist::call_peer` (kept local; see future
/// PH-BRIDGE-DIAL-REFACTOR).
async fn call_peer(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
) -> Result<String, (StatusCode, Json<ApiError>)> {
    let mesh = state.mesh_client.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: "bridge mesh client not initialized (peer discovery failed at startup)".into(),
        }),
    ))?;
    let envelope = build_request_with_tenant(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        state.cfg.transport.deadline_secs,
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
                error: "unexpected stream response from tool.browser.list_sessions".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sessions_typical_body() {
        let body = "abc1234567890def\t1716000000\thttps://example.com/\tconnected\n\
                    deadbeefdeadbeef\t1716000100\t-\tunconnected\n\
                    count=2\n";
        let rows = parse_sessions(body);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].session_id, "abc1234567890def");
        assert_eq!(rows[0].opened_at, 1716000000);
        assert_eq!(rows[0].current_url.as_deref(), Some("https://example.com/"));
        assert_eq!(rows[0].status, "connected");
        // "-" sentinel converts to None.
        assert_eq!(rows[1].session_id, "deadbeefdeadbeef");
        assert_eq!(rows[1].current_url, None);
        assert_eq!(rows[1].status, "unconnected");
    }

    #[test]
    fn parse_sessions_empty_body() {
        let (rows, _empty) = (parse_sessions(""), 0usize);
        assert!(rows.is_empty());
    }

    #[test]
    fn parse_sessions_only_count_trailer() {
        let rows = parse_sessions("count=0\n");
        assert!(rows.is_empty());
    }

    #[test]
    fn parse_sessions_drops_malformed_rows() {
        // Missing columns + non-numeric opened_at both dropped.
        let body = "ok-id\t1716\thttps://x/\tconnected\n\
                    broken\tonly-two\n\
                    notanumber\twrong\thttps://y/\tunconnected\n\
                    count=3\n";
        let rows = parse_sessions(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, "ok-id");
    }

    #[test]
    fn parse_sessions_handles_extra_columns_gracefully() {
        // Forward-compat: runtime might grow a fifth column
        // (e.g. page_title) — current bridge picks up the first
        // four cleanly.
        let body = "id\t1716\thttps://x/\tconnected\tfuture-col\ncount=1\n";
        let rows = parse_sessions(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "connected");
    }

    #[test]
    fn parse_sessions_empty_url_treated_as_none() {
        // Belt-and-suspenders: if the runtime ever emits empty
        // instead of "-", both forms map to None.
        let body = "id\t1716\t\tconnected\ncount=1\n";
        let rows = parse_sessions(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].current_url, None);
    }

    #[test]
    fn browser_session_row_skips_current_url_when_none() {
        // The JSON omits `current_url` for sessions that have
        // not yet navigated, so the dashboard can render the
        // absence rather than a hyphen literal.
        let row = BrowserSessionRow {
            session_id: "id".into(),
            opened_at: 1,
            current_url: None,
            status: "unconnected".into(),
        };
        let s = serde_json::to_string(&row).unwrap();
        assert!(!s.contains("current_url"));
    }

    #[test]
    fn browser_session_row_includes_current_url_when_some() {
        let row = BrowserSessionRow {
            session_id: "id".into(),
            opened_at: 1,
            current_url: Some("https://example.com/".into()),
            status: "connected".into(),
        };
        let s = serde_json::to_string(&row).unwrap();
        assert!(s.contains(r#""current_url":"https://example.com/""#));
    }
}
