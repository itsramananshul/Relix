//! `/v1/sessions` and `/v1/sessions/{id}` — operator surface
//! over the two-sink observability data.
//!
//! Three endpoints:
//!
//! - `GET /v1/sessions?status=...&limit=...` — list
//!   sessions with optional `running` / `completed` /
//!   `stalled` filter.
//! - `GET /v1/sessions/{id}` — full session timeline.
//! - `GET /v1/sessions/{id}/content/{event_id}` — fetch one
//!   event's content from Sink B. Requires the
//!   `X-Relix-Elevated: true` header so a logged-in dashboard
//!   user can't accidentally read prompt / response text
//!   they're not entitled to see.

use axum::Json;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use relix_runtime::observability::{
    ContentEvent, ObservabilityContext, SessionDebugger, SessionSummary, SessionTimeline,
};
use serde::{Deserialize, Serialize};

use crate::config::AppState;

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize)]
pub struct ListSessionsQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    20
}

#[derive(Debug, Serialize)]
pub struct ListSessionsResponse {
    pub sessions: Vec<SessionSummary>,
    pub count: usize,
}

type HandlerError = (StatusCode, Json<ApiError>);

fn debugger(ctx: &ObservabilityContext) -> SessionDebugger {
    SessionDebugger::new(ctx.metadata.clone(), ctx.content.clone())
}

pub(crate) fn list_logic(
    ctx: &ObservabilityContext,
    q: &ListSessionsQuery,
) -> Result<ListSessionsResponse, HandlerError> {
    let dbg = debugger(ctx);
    let status = q.status.as_deref();
    let sessions = dbg.list_sessions(status, q.limit).map_err(sink_err)?;
    Ok(ListSessionsResponse {
        count: sessions.len(),
        sessions,
    })
}

pub(crate) fn show_logic(
    ctx: &ObservabilityContext,
    session_id: &str,
) -> Result<SessionTimeline, HandlerError> {
    let dbg = debugger(ctx);
    match dbg.session_timeline(session_id).map_err(sink_err)? {
        Some(t) => Ok(t),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: format!("session {session_id} not found"),
            }),
        )),
    }
}

pub(crate) fn content_logic(
    ctx: &ObservabilityContext,
    elevated: bool,
    event_id: &str,
) -> Result<ContentEvent, HandlerError> {
    if !elevated {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiError {
                error: "content endpoint requires `X-Relix-Elevated: true` header".into(),
            }),
        ));
    }
    match ctx.content.get(event_id).map_err(sink_err)? {
        Some(c) => Ok(c),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: format!("no content recorded for event {event_id}"),
            }),
        )),
    }
}

fn sink_err(e: relix_runtime::observability::SinkError) -> HandlerError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError {
            error: format!("observability: {e}"),
        }),
    )
}

fn elevated_from_headers(headers: &HeaderMap) -> bool {
    headers
        .get("x-relix-elevated")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListSessionsQuery>,
) -> Result<Json<ListSessionsResponse>, HandlerError> {
    list_logic(&state.observability, &q).map(Json)
}

pub async fn show(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<SessionTimeline>, HandlerError> {
    show_logic(&state.observability, &id).map(Json)
}

pub async fn content(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath((_session_id, event_id)): AxumPath<(String, String)>,
) -> Result<Json<ContentEvent>, HandlerError> {
    let elevated = elevated_from_headers(&headers);
    content_logic(&state.observability, elevated, &event_id).map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_runtime::observability::{ContentEvent, MetadataEvent};

    fn make_ctx() -> ObservabilityContext {
        ObservabilityContext::in_memory()
    }

    fn evt(event_id: &str, session: &str, ty: &str, ts: i64) -> MetadataEvent {
        MetadataEvent {
            event_id: event_id.into(),
            session_id: session.into(),
            agent_id: "alice".into(),
            event_type: ty.into(),
            timestamp_unix: ts,
            latency_ms: Some(10),
            token_count: None,
            cost_cents: Some(1),
            error_type: None,
            tool_name: None,
            model_name: Some("gpt-test".into()),
            success: true,
        }
    }

    #[test]
    fn show_returns_session_timeline_when_present() {
        let ctx = make_ctx();
        ctx.metadata
            .record(&evt("a", "s1", "model_call", 100))
            .unwrap();
        let resp = show_logic(&ctx, "s1").unwrap();
        assert_eq!(resp.session_id, "s1");
        assert_eq!(resp.events.len(), 1);
    }

    #[test]
    fn show_returns_404_for_unknown_session() {
        let ctx = make_ctx();
        let err = show_logic(&ctx, "missing").unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn list_filters_by_status() {
        let ctx = make_ctx();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        ctx.metadata
            .record(&evt("a", "s1", "model_call", now - 10))
            .unwrap();
        let resp = list_logic(
            &ctx,
            &ListSessionsQuery {
                status: Some("running".into()),
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(resp.count, 1);
        assert_eq!(resp.sessions[0].session_id, "s1");
        let none = list_logic(
            &ctx,
            &ListSessionsQuery {
                status: Some("completed".into()),
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(none.count, 0);
    }

    #[test]
    fn content_endpoint_requires_elevated_header() {
        let ctx = make_ctx();
        ctx.content
            .record(&ContentEvent {
                event_id: "a".into(),
                content_type: "prompt".into(),
                content: "hello".into(),
                redacted: false,
                timestamp_unix: 0,
            })
            .unwrap();
        // Without elevated → 403.
        let err = content_logic(&ctx, false, "a").unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert!(err.1.error.contains("X-Relix-Elevated"));
        // With elevated → 200 with the content.
        let ok = content_logic(&ctx, true, "a").unwrap();
        assert_eq!(ok.content, "hello");
    }

    #[test]
    fn content_endpoint_returns_404_when_event_has_no_content() {
        let ctx = make_ctx();
        let err = content_logic(&ctx, true, "missing").unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn elevated_header_parses_case_insensitively() {
        let mut h = HeaderMap::new();
        h.insert("x-relix-elevated", "TRUE".parse().unwrap());
        assert!(elevated_from_headers(&h));
        h.insert("x-relix-elevated", "TrUe".parse().unwrap());
        assert!(elevated_from_headers(&h));
        h.insert("x-relix-elevated", "false".parse().unwrap());
        assert!(!elevated_from_headers(&h));
        // Missing header → false.
        let empty = HeaderMap::new();
        assert!(!elevated_from_headers(&empty));
    }
}
