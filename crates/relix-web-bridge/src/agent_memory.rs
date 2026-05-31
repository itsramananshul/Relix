//! W2-MEMORY-3 — HTTP proxy for `memory.agent_read`. Lets the
//! dashboard and the `relix-cli ops agent-memory` mirror inspect
//! the persistent agent + user memory for a `subject_id` without
//! a libp2p identity bundle on disk.
//!
//! Two endpoints:
//!
//! - `GET /v1/memory/agent?subject_id=<id>&peer=<alias>` —
//!   proxies `memory.agent_read`. Returns JSON:
//!
//!   ```json
//!   {
//!     "peer": "memory",
//!     "subject_id": "abc123...",
//!     "agent_memory": "rust uses cargo§python uses pip",
//!     "user_memory":  "prefers concise replies",
//!     "agent_chars": 30,
//!     "user_chars":  23
//!   }
//!   ```
//!
//!   `peer` defaults to `"memory"`; the dashboard's inspector
//!   exposes both inputs so an operator running a non-default
//!   alias can still hit it.
//!
//! Read-only. Memory writes happen via the agent's `memory`
//! tool path inside ai.chat sessions, never via the dashboard.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "memory";

#[derive(Debug, Deserialize)]
pub struct AgentMemoryQuery {
    #[serde(default)]
    pub peer: Option<String>,
    /// Required. The `subject_id` whose memory to read.
    pub subject_id: String,
}

#[derive(Debug, Serialize)]
pub struct AgentMemoryResponse {
    pub peer: String,
    pub subject_id: String,
    pub agent_memory: String,
    pub user_memory: String,
    pub agent_chars: usize,
    pub user_chars: usize,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub async fn agent_memory(
    State(state): State<AppState>,
    Query(q): Query<AgentMemoryQuery>,
) -> Result<Json<AgentMemoryResponse>, (StatusCode, Json<ApiError>)> {
    // GROUP 1 PHASE 1A: a caller may only read their OWN agent /
    // user memory. The subject is the AUTHENTICATED caller; the
    // query `subject_id` may only agree with it (else it is a
    // spoof attempt to read another subject's private memory →
    // 403). Identity comes from the authenticated principal
    // channel, never the query string.
    let subject_id = crate::tenant::require_caller_subject(Some(&q.subject_id)).map_err(|e| {
        let (code, msg) = match e {
            crate::tenant::SubjectError::Unauthenticated => (
                StatusCode::UNAUTHORIZED,
                "caller subject not authenticated; identity is derived from the \
                 X-Relix-Subject principal channel, not the query string"
                    .to_string(),
            ),
            crate::tenant::SubjectError::Forbidden {
                claimed,
                authenticated,
            } => (
                StatusCode::FORBIDDEN,
                format!(
                    "subject `{claimed}` does not match authenticated caller \
                     `{authenticated}`; a caller may only read their own memory"
                ),
            ),
        };
        (code, Json(ApiError { error: msg }))
    })?;
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER).to_string();
    let body = call_peer_bytes(&state, &peer, "memory.agent_read", subject_id.as_bytes()).await?;
    let (agent_memory, user_memory) = parse_body(&body).ok_or((
        StatusCode::BAD_GATEWAY,
        Json(ApiError {
            error: format!(
                "memory peer returned malformed agent_read body ({} bytes)",
                body.len()
            ),
        }),
    ))?;
    let agent_chars = agent_memory.chars().count();
    let user_chars = user_memory.chars().count();
    Ok(Json(AgentMemoryResponse {
        peer,
        subject_id,
        agent_memory,
        user_memory,
        agent_chars,
        user_chars,
    }))
}

/// Strict length-prefix parser matching the memory node's
/// `memory.agent_read` wire format. `None` on any malformed
/// input — caller maps to 502.
pub fn parse_body(body: &[u8]) -> Option<(String, String)> {
    let nl = body.iter().position(|b| *b == b'\n')?;
    let header = std::str::from_utf8(&body[..nl]).ok()?;
    let (agent_kv, user_kv) = header.split_once('|')?;
    let agent_len = agent_kv
        .strip_prefix("agent_bytes=")?
        .parse::<usize>()
        .ok()?;
    let user_len = user_kv.strip_prefix("user_bytes=")?.parse::<usize>().ok()?;
    let payload = &body[nl + 1..];
    if payload.len() != agent_len + user_len {
        return None;
    }
    let agent = std::str::from_utf8(&payload[..agent_len]).ok()?.to_string();
    let user = std::str::from_utf8(&payload[agent_len..agent_len + user_len])
        .ok()?
        .to_string();
    Some((agent, user))
}

/// Mesh-call helper that returns the raw response bytes. Lifted
/// from the policy_denials proxy pattern; the response payload
/// here is binary-safe (length-prefixed bytes) so we cannot
/// reuse the string-typed call_peer.
async fn call_peer_bytes(
    state: &AppState,
    alias: &str,
    method: &str,
    arg: &[u8],
) -> Result<Vec<u8>, (StatusCode, Json<ApiError>)> {
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
        ResponseResult::Ok(body) => Ok(body.to_vec()),
        ResponseResult::Err(env) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("responder err kind={} cause={}", env.kind, env.cause),
            }),
        )),
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response from memory.agent_read".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_typical_body() {
        let body = b"agent_bytes=5|user_bytes=6\nhelloworld!";
        let (a, u) = parse_body(body).unwrap();
        assert_eq!(a, "hello");
        assert_eq!(u, "world!");
    }

    #[test]
    fn parse_empty_body() {
        let body = b"agent_bytes=0|user_bytes=0\n";
        let (a, u) = parse_body(body).unwrap();
        assert_eq!(a, "");
        assert_eq!(u, "");
    }

    #[test]
    fn parse_rejects_truncated() {
        let body = b"agent_bytes=5|user_bytes=5\nhi";
        assert!(parse_body(body).is_none());
    }

    #[test]
    fn parse_rejects_missing_header() {
        assert!(parse_body(b"no header here").is_none());
    }

    #[test]
    fn parse_rejects_malformed_length() {
        let body = b"agent_bytes=NaN|user_bytes=0\n";
        assert!(parse_body(body).is_none());
    }
}
