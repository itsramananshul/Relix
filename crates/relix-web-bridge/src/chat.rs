//! Native chat endpoints: `POST /chat` (JSON) and `POST /chat/stream` (SSE).

use std::time::Duration;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Sse, sse::KeepAlive},
};
use serde::{Deserialize, Serialize};

use crate::config::AppState;
use crate::flow::{FlowExecError, execute_chat_flow, execute_chat_with_tool_flow};
use crate::sse::build_chunked_sse;

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    pub session_id: String,
    pub message: String,
    #[serde(default)]
    pub workspace_lease_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChatResponse {
    pub reply: String,
    pub flow_id: String,
    pub trace_id: String,
    pub flow_log: String,
    /// Coordinator-side Task id when persistence was wired and the
    /// `task.create` call succeeded. `None` when the coordinator is
    /// absent (`[coordinator]` missing in bridge config) or any
    /// coordinator call failed (B1.9 fail-soft).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_lease_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
    pub flow_id: Option<String>,
    pub flow_log: Option<String>,
}

pub async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

pub async fn chat(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, (StatusCode, Json<ErrorResponse>)> {
    match execute_chat_flow(
        &state,
        &req.session_id,
        &req.message,
        req.workspace_lease_id.as_deref(),
    )
    .await
    {
        Ok(o) => Ok(Json(ChatResponse {
            reply: o.reply,
            flow_id: o.flow_id,
            trace_id: o.trace_id,
            flow_log: o.flow_log_path,
            task_id: o.task_id,
            workspace_lease_id: o.workspace_lease_id,
            workspace_path: o.workspace_path,
        })),
        Err(e) => Err(exec_error_to_http(e)),
    }
}

#[derive(Debug, Deserialize)]
pub struct ChatWithToolRequest {
    pub session_id: String,
    pub message: String,
    /// External http(s) URL the tool node will fetch. The bridge applies its
    /// own substitution-boundary validator; the SSRF gate lives on the tool
    /// node (`tool::security::resolve_safe_url`).
    pub url: String,
    #[serde(default)]
    pub workspace_lease_id: Option<String>,
}

/// `POST /chat_with_tool` — tool-augmented chat flow.
///
/// Returns 404 when the bridge is not configured with `[flow]
/// tool_template_path`. Bridge responsibility is template selection ONLY: SOL
/// owns orchestration (memory.write → memory.read → tool.web_fetch →
/// ai.chat → memory.write).
pub async fn chat_with_tool(
    State(state): State<AppState>,
    Json(req): Json<ChatWithToolRequest>,
) -> Result<Json<ChatResponse>, (StatusCode, Json<ErrorResponse>)> {
    if state.tool_template.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "tool flow not configured (set [flow] tool_template_path)".into(),
                flow_id: None,
                flow_log: None,
            }),
        ));
    }
    match execute_chat_with_tool_flow(
        &state,
        &req.session_id,
        &req.message,
        &req.url,
        req.workspace_lease_id.as_deref(),
    )
    .await
    {
        Ok(o) => Ok(Json(ChatResponse {
            reply: o.reply,
            flow_id: o.flow_id,
            trace_id: o.trace_id,
            flow_log: o.flow_log_path,
            task_id: o.task_id,
            workspace_lease_id: o.workspace_lease_id,
            workspace_path: o.workspace_path,
        })),
        Err(e) => Err(exec_error_to_http(e)),
    }
}

/// `POST /chat/stream` — bridge-level SSE (SIMP-019).
///
/// Output frames:
///
/// ```text
/// event: chunk
/// data: <slice of the final reply>
///
/// event: done
/// data: {"flow_id":"…","trace_id":"…","flow_log":"…"}
/// ```
pub async fn chat_stream(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<ErrorResponse>)> {
    let outcome = execute_chat_flow(
        &state,
        &req.session_id,
        &req.message,
        req.workspace_lease_id.as_deref(),
    )
    .await
    .map_err(exec_error_to_http)?;

    let done_payload = serde_json::json!({
        "flow_id": outcome.flow_id,
        "trace_id": outcome.trace_id,
        "flow_log": outcome.flow_log_path,
        "task_id": outcome.task_id,
        "workspace_lease_id": outcome.workspace_lease_id,
        "workspace_path": outcome.workspace_path,
    })
    .to_string();

    let stream = build_chunked_sse(
        outcome.reply,
        state.cfg.sse.chunk_bytes,
        Duration::from_millis(state.cfg.sse.chunk_delay_ms),
        done_payload,
    );

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

pub fn exec_error_to_http(e: FlowExecError) -> (StatusCode, Json<ErrorResponse>) {
    match e {
        FlowExecError::InvalidInput(msg) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: msg,
                flow_id: None,
                flow_log: None,
            }),
        ),
        FlowExecError::Transport(msg) => (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: format!("mesh transport: {msg}"),
                flow_id: None,
                flow_log: None,
            }),
        ),
        FlowExecError::Unavailable(msg) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: msg,
                flow_id: None,
                flow_log: None,
            }),
        ),
        FlowExecError::Internal(msg) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: msg,
                flow_id: None,
                flow_log: None,
            }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_accepts_optional_workspace_lease_id() {
        let req: ChatRequest = serde_json::from_str(
            r#"{"session_id":"s1","message":"hi","workspace_lease_id":"wsl_abc"}"#,
        )
        .expect("request parses");
        assert_eq!(req.workspace_lease_id.as_deref(), Some("wsl_abc"));
    }

    #[test]
    fn unavailable_flow_error_maps_to_503() {
        let (status, body) = exec_error_to_http(FlowExecError::Unavailable(
            "coordinator task persistence is required but unavailable".into(),
        ));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.error.contains("task persistence"));
    }
}
