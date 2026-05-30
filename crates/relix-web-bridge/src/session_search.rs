//! `GET /v1/memory/sessions/search` — bridge proxy onto the
//! memory node's `memory.session_search` capability (which in
//! turn proxies the coordinator's `task.session_search`).
//!
//! Wire shape:
//!
//! ```text
//! GET /v1/memory/sessions/search?q=<query>&subject_id=<id>&limit=<n>
//! → 200 { results: [...], total: N, query: "...", subject_id: "..." }
//! → 400 when q is missing or empty
//! → 503 when no memory peer is wired
//! → 502 when the memory / coordinator peer returns a responder error
//! ```
//!
//! Returns real data sourced from the chronicle. No
//! placeholders, no bridge_note, no synth.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::AppState;

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 100;
const DEFAULT_PEER: &str = "memory";

#[derive(Debug, Deserialize)]
pub struct SessionSearchQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub subject_id: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional override for the memory peer alias. Defaults
    /// to `"memory"` so the standard mesh layout works
    /// without setting it.
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SessionSearchResponse {
    pub results: Vec<Value>,
    pub total: usize,
    pub query: String,
    pub subject_id: String,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

type HttpErr = (StatusCode, Json<ApiError>);

pub async fn search(
    State(state): State<AppState>,
    Query(q): Query<SessionSearchQuery>,
) -> Result<Json<SessionSearchResponse>, HttpErr> {
    let query =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(ApiError {
                        error: "q query param required".into(),
                    }),
                )
            })?
            .to_string();
    let subject_id = q.subject_id.unwrap_or_default();
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
    let peer = q.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());

    let arg = format!("{subject_id}|{query}|{limit}");
    let body = call_memory_peer(&state, &peer, arg.as_bytes()).await?;
    let parsed: Vec<Value> = serde_json::from_str(&body).map_err(|e| {
        (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("memory.session_search returned invalid JSON: {e}"),
            }),
        )
    })?;
    Ok(Json(SessionSearchResponse {
        total: parsed.len(),
        results: parsed,
        query,
        subject_id,
    }))
}

async fn call_memory_peer(state: &AppState, alias: &str, arg: &[u8]) -> Result<String, HttpErr> {
    let mesh = state.mesh_client.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ApiError {
            error: "bridge mesh client not initialized (peer discovery failed at startup)".into(),
        }),
    ))?;
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(10, 60);
    let envelope = build_request_with_tenant(
        "memory.session_search",
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
            StatusCode::SERVICE_UNAVAILABLE
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
            // PEER_UNREACHABLE on the responder side maps back
            // to 503 so the dashboard / CLI surface the same
            // status it'd see for transport-level absence.
            let status = if env.kind == relix_core::types::error_kinds::PEER_UNREACHABLE {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::BAD_GATEWAY
            };
            Err((
                status,
                Json(ApiError {
                    error: format!("responder err kind={} cause={}", env.kind, env.cause),
                }),
            ))
        }
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response from memory.session_search".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Validate the query-string parsing + limit-clamping
    /// logic that runs before any mesh call. Pure logic test;
    /// no peer needed.
    fn classify(q: &SessionSearchQuery) -> Result<(String, String, usize), (StatusCode, String)> {
        let query =
            q.q.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or((
                    StatusCode::BAD_REQUEST,
                    "q query param required".to_string(),
                ))?
                .to_string();
        let subject_id = q.subject_id.clone().unwrap_or_default();
        let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        Ok((query, subject_id, limit))
    }

    #[test]
    fn missing_q_returns_400() {
        let q = SessionSearchQuery {
            q: None,
            subject_id: None,
            limit: None,
            peer: None,
        };
        let err = classify(&q).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("q query param required"));
    }

    #[test]
    fn empty_q_returns_400() {
        let q = SessionSearchQuery {
            q: Some("   ".into()),
            subject_id: None,
            limit: None,
            peer: None,
        };
        let err = classify(&q).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn limit_above_max_is_capped_to_100() {
        let q = SessionSearchQuery {
            q: Some("needle".into()),
            subject_id: None,
            limit: Some(500),
            peer: None,
        };
        let (_, _, limit) = classify(&q).unwrap();
        assert_eq!(limit, MAX_LIMIT);
    }

    #[test]
    fn default_limit_is_20() {
        let q = SessionSearchQuery {
            q: Some("needle".into()),
            subject_id: None,
            limit: None,
            peer: None,
        };
        let (_, _, limit) = classify(&q).unwrap();
        assert_eq!(limit, DEFAULT_LIMIT);
    }

    #[test]
    fn missing_subject_id_defaults_to_empty_string() {
        let q = SessionSearchQuery {
            q: Some("needle".into()),
            subject_id: None,
            limit: Some(5),
            peer: None,
        };
        let (query, subject_id, limit) = classify(&q).unwrap();
        assert_eq!(query, "needle");
        assert_eq!(subject_id, "");
        assert_eq!(limit, 5);
    }

    #[test]
    fn response_shape_round_trips() {
        let resp = SessionSearchResponse {
            results: vec![serde_json::json!({
                "session_id": "sess-1",
                "role": "user",
                "content": "hi",
                "timestamp_unix": 1,
                "snippet": "hi",
                "score": 1.0,
            })],
            total: 1,
            query: "hi".into(),
            subject_id: "alice".into(),
        };
        let j = serde_json::to_string(&resp).unwrap();
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["total"], 1);
        assert_eq!(v["query"], "hi");
        assert_eq!(v["subject_id"], "alice");
        assert_eq!(v["results"][0]["session_id"], "sess-1");
    }
}
