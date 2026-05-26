//! `GET /v1/sessions/export` — conversation history export.
//!
//! Aggregates the coordinator's `task.list_cursor` +
//! `task.events` data into the canonical session shape the
//! `relix export` CLI consumes (and any future SDK / dashboard
//! download path). Three output formats:
//!
//! - `json` (default) — machine-readable array of session
//!   objects matching `SessionExport`.
//! - `markdown` — human-readable transcript with headings +
//!   timestamps.
//! - `csv` — one row per message, columns documented in the
//!   CLI's `--help` text.
//!
//! Scope (one of, mutually exclusive):
//! - `session=<id>` — one session.
//! - `agent=<name>` — every session for an agent (subject_id
//!   substring match, since the alpha doesn't have a dedicated
//!   agent → session index).
//! - `all=1` — everything.
//!
//! Honest scope: the alpha bridge does not have a separate
//! per-session ledger — sessions are derived from chat-flow
//! tasks. So a "session" today = one task (created by /chat or
//! /v1/chat/completions). Future schema work can fold multiple
//! tasks into a single conversation, at which point this
//! endpoint's renderer becomes the natural integration point.

use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::config::AppState;

#[derive(Debug, Deserialize)]
pub struct ExportQuery {
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub all: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
}

/// Canonical export shape. Mirrors the CLI's `SessionExport`
/// type so the two sides agree on field names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionExport {
    pub session_id: String,
    #[serde(default)]
    pub agent: String,
    pub start_time: i64,
    pub end_time: i64,
    pub messages: Vec<SessionMessage>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessage {
    pub timestamp: i64,
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub token_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub timestamp: i64,
    pub tool_name: String,
    #[serde(default)]
    pub args: String,
}

pub async fn export(State(state): State<AppState>, Query(q): Query<ExportQuery>) -> Response {
    // Scope validation — exactly one of session / agent / all.
    let scope = match (
        q.session.as_deref().filter(|s| !s.is_empty()),
        q.agent.as_deref().filter(|s| !s.is_empty()),
        q.all.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(s), None, None) => ExportScope::Session(s.to_string()),
        (None, Some(a), None) => ExportScope::Agent(a.to_string()),
        (None, None, Some(_)) => ExportScope::All,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                "specify exactly one of session=, agent=, or all=1\n",
            )
                .into_response();
        }
    };

    // W5: session= scope dispatches to the coordinator's
    // `task.session_export` capability for real turn-by-turn
    // history. agent= / all= still return the scaffold today;
    // those layouts grow when the coordinator gains a
    // session-by-agent lookup.
    let sessions = match &scope {
        ExportScope::Session(sid) => match fetch_real_session(&state, sid).await {
            Ok(s) => s,
            Err(resp) => return resp,
        },
        _ => synth_export(&state, &scope),
    };

    let format = q.format.as_deref().unwrap_or("json");
    match format {
        "markdown" | "md" => {
            let body = render_markdown(&sessions);
            text_response("text/markdown; charset=utf-8", body)
        }
        "csv" => {
            let body = render_csv(&sessions);
            text_response("text/csv; charset=utf-8", body)
        }
        _ => {
            // JSON default.
            let body = serde_json::to_string_pretty(&sessions).unwrap_or_else(|_| "[]".to_string());
            text_response("application/json", body)
        }
    }
}

fn text_response(content_type: &'static str, body: String) -> Response {
    let mut r = body.into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    // Browsers should treat the response as an attachment so
    // `Save as…` works directly from a dashboard "Download"
    // button. Filename suffix matches Content-Type so OSes pick
    // the right opener.
    let ext = match content_type {
        s if s.starts_with("text/markdown") => "md",
        s if s.starts_with("text/csv") => "csv",
        _ => "json",
    };
    let _ = r.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"relix-export.{ext}\""))
            .unwrap_or(HeaderValue::from_static("attachment")),
    );
    r
}

#[derive(Debug)]
enum ExportScope {
    Session(String),
    Agent(String),
    All,
}

/// W5: pull turn-by-turn history for `session_id` from the
/// coordinator's `task.session_export` capability and project
/// it into the canonical [`SessionExport`] shape.
async fn fetch_real_session(
    state: &AppState,
    session_id: &str,
) -> Result<Vec<SessionExport>, Response> {
    let Some(rec) = state.task_recorder.as_ref() else {
        // No coordinator configured — be honest and surface a
        // 503, not a fake stub.
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "no coordinator configured; session export unavailable\n",
        )
            .into_response());
    };
    let body = rec.session_export(session_id).await.map_err(|e| {
        (StatusCode::BAD_GATEWAY, format!("coord call failed: {e}\n")).into_response()
    })?;
    #[derive(Deserialize)]
    struct ChatTurnWire {
        #[allow(dead_code)]
        session_id: String,
        role: String,
        content: String,
        timestamp_unix: i64,
    }
    let turns: Vec<ChatTurnWire> = serde_json::from_str(&body).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            format!("coord task.session_export returned invalid JSON: {e}\n"),
        )
            .into_response()
    })?;
    let (start_time, end_time) = match (turns.first(), turns.last()) {
        (Some(f), Some(l)) => (f.timestamp_unix, l.timestamp_unix),
        _ => (0, 0),
    };
    let messages: Vec<SessionMessage> = turns
        .into_iter()
        .map(|t| SessionMessage {
            timestamp: t.timestamp_unix,
            role: t.role,
            content: t.content,
            token_count: 0,
        })
        .collect();
    Ok(vec![SessionExport {
        session_id: session_id.to_string(),
        agent: String::new(),
        start_time,
        end_time,
        messages,
        tool_calls: Vec::new(),
        cost_usd: 0.0,
    }])
}

/// Synthesise a minimal session export from bridge-level state.
/// Returns a single placeholder session today; the contract is
/// stable so a richer future implementation (driven by a real
/// `task.session_export` coordinator capability) lands without
/// changing the response shape.
fn synth_export(state: &AppState, scope: &ExportScope) -> Vec<SessionExport> {
    let session_id = match scope {
        ExportScope::Session(s) => s.clone(),
        ExportScope::Agent(a) => format!("agent:{a}:placeholder"),
        ExportScope::All => "all:placeholder".to_string(),
    };
    let now = state.started_at.max(unix_secs());
    vec![SessionExport {
        session_id,
        agent: match scope {
            ExportScope::Agent(a) => a.clone(),
            _ => String::new(),
        },
        start_time: state.started_at,
        end_time: now,
        messages: vec![SessionMessage {
            timestamp: state.started_at,
            role: "system".into(),
            content: format!(
                "Export endpoint scaffold — bridge {}, started at unix {}. \
                 Full per-session message history lands when the \
                 task.session_export coordinator capability ships.",
                env!("CARGO_PKG_VERSION"),
                state.started_at,
            ),
            token_count: 0,
        }],
        tool_calls: Vec::new(),
        cost_usd: 0.0,
    }]
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Render a slice of `SessionExport` as Markdown. Pure function,
/// exported for tests.
pub fn render_markdown(sessions: &[SessionExport]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "# Relix conversation export\n");
    let _ = writeln!(out, "_{} session(s)_\n", sessions.len());
    for s in sessions {
        let _ = writeln!(out, "---\n");
        let _ = writeln!(out, "## Session `{}`\n", s.session_id);
        if !s.agent.is_empty() {
            let _ = writeln!(out, "- **Agent:** {}", s.agent);
        }
        let _ = writeln!(
            out,
            "- **Started:** unix={}\n- **Ended:**   unix={}\n- **Cost:**    ${:.4}\n",
            s.start_time, s.end_time, s.cost_usd,
        );
        for m in &s.messages {
            let _ = writeln!(out, "### [{} · {}]\n\n{}\n", m.timestamp, m.role, m.content);
        }
    }
    out
}

/// Render a slice of `SessionExport` as CSV.
pub fn render_csv(sessions: &[SessionExport]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str("session_id,timestamp,role,content,tool_name,token_count,cost_usd\n");
    for s in sessions {
        for m in &s.messages {
            let _ = writeln!(
                out,
                "{},{},{},{},,{},{:.6}",
                csv_escape(&s.session_id),
                m.timestamp,
                csv_escape(&m.role),
                csv_escape(&m.content),
                m.token_count,
                s.cost_usd,
            );
        }
        for t in &s.tool_calls {
            let _ = writeln!(
                out,
                "{},{},tool,{},{},0,{:.6}",
                csv_escape(&s.session_id),
                t.timestamp,
                csv_escape(&t.args),
                csv_escape(&t.tool_name),
                s.cost_usd,
            );
        }
    }
    out
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        let body = s.replace('"', "\"\"");
        format!("\"{body}\"")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SessionExport {
        SessionExport {
            session_id: "s1".into(),
            agent: "alice".into(),
            start_time: 1_700_000_000,
            end_time: 1_700_000_100,
            messages: vec![SessionMessage {
                timestamp: 1_700_000_001,
                role: "user".into(),
                content: "hi".into(),
                token_count: 1,
            }],
            tool_calls: Vec::new(),
            cost_usd: 0.001,
        }
    }

    #[test]
    fn markdown_render_has_session_landmarks() {
        let md = render_markdown(&[sample()]);
        assert!(md.contains("# Relix conversation export"));
        assert!(md.contains("## Session `s1`"));
        assert!(md.contains("**Agent:** alice"));
        assert!(md.contains("hi"));
    }

    #[test]
    fn csv_render_has_header_and_one_row_per_message() {
        let csv = render_csv(&[sample()]);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 2); // header + 1 message
        assert!(lines[0].starts_with("session_id,timestamp,role,"));
        assert!(lines[1].contains("user"));
    }

    #[test]
    fn json_round_trips() {
        let s = sample();
        let json = serde_json::to_string(&s).unwrap();
        let back: SessionExport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, "s1");
    }
}
