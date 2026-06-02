//! HTTP proxies for the memory node's vector-embedding surface.
//!
//! Three endpoints — the bridge does not store embeddings or
//! talk to an embedding provider directly; each handler proxies
//! to the configured memory peer over the mesh.
//!
//! - `POST /v1/memory/embed`     → `memory.embed` (one chunk).
//! - `POST /v1/memory/search`    → `memory.search` (semantic).
//! - `POST /v1/memory/embed_all` → `memory.embed_all` (background
//!   re-embed of one subject's persistent memory).

use axum::{Json, extract::State, http::StatusCode};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::activity::{ToolInvocationActivity, append_tool_invocation_activity};
use crate::config::AppState;
use crate::tenant::{DEFAULT_TENANT, current_subject};

const DEFAULT_PEER: &str = "memory";
const MAX_TEXT_BYTES: usize = 8 * 1024;
const MAX_QUERY_BYTES: usize = 2 * 1024;

#[derive(Debug, Deserialize)]
pub struct EmbedRequest {
    pub subject_id: String,
    pub target: String,
    pub text: String,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EmbedResponse {
    pub embedding_id: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub already_present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub subject_id: String,
    pub target: String,
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub embedding_id: String,
    pub score: f32,
    pub chunk_text: String,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub results: Vec<SearchHit>,
    pub count: usize,
}

#[derive(Debug, Deserialize)]
pub struct EmbedAllRequest {
    pub subject_id: String,
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct EmbedAllResponse {
    pub ok: bool,
    pub chunks_embedded: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub async fn embed(
    State(state): State<AppState>,
    Json(req): Json<EmbedRequest>,
) -> Result<Json<EmbedResponse>, (StatusCode, Json<ApiError>)> {
    if req.subject_id.trim().is_empty() || req.target.trim().is_empty() || req.text.is_empty() {
        return Err(bad_request(
            "subject_id, target, and text are required".into(),
        ));
    }
    if req.target != "agent" && req.target != "user" {
        return Err(bad_request("target must be 'agent' or 'user'".into()));
    }
    if req.text.len() > MAX_TEXT_BYTES {
        return Err(bad_request(format!(
            "text exceeds {MAX_TEXT_BYTES}-byte cap"
        )));
    }
    let task_id = clean_optional_id(req.task_id.as_deref(), "task_id")?;
    let run_id = clean_optional(req.run_id.as_deref());
    let peer = req.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let detail = embed_detail(&req.subject_id, &req.target, req.text.len());
    let arg = format!("{}|{}|{}", req.subject_id, req.target, req.text);
    let body = match call_peer_string(
        &state,
        &peer,
        "memory.embed",
        arg.as_bytes(),
        task_id.as_deref(),
    )
    .await
    {
        Ok(body) => {
            record_embedding_activity(
                &state,
                EmbeddingActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "memory.embed",
                    decision: "ok",
                    detail: &detail,
                },
            );
            body
        }
        Err(err) => {
            record_embedding_activity(
                &state,
                EmbeddingActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "memory.embed",
                    decision: "err",
                    detail: &detail,
                },
            );
            return Err(err);
        }
    };
    let trimmed = body.trim_end_matches('\n');
    // Two return shapes:
    //   `embedding_id=<id>` — newly stored
    //   `ok|embedding_id=<id>` — dedup
    let (already, id_part) = if let Some(rest) = trimmed.strip_prefix("ok|") {
        (true, rest)
    } else {
        (false, trimmed)
    };
    let embedding_id = id_part
        .strip_prefix("embedding_id=")
        .unwrap_or(id_part)
        .to_string();
    Ok(Json(EmbedResponse {
        embedding_id,
        already_present: already,
        task_id,
        run_id,
    }))
}

pub async fn search(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, (StatusCode, Json<ApiError>)> {
    if req.subject_id.trim().is_empty() || req.target.trim().is_empty() || req.query.is_empty() {
        return Err(bad_request(
            "subject_id, target, and query are required".into(),
        ));
    }
    if req.target != "agent" && req.target != "user" {
        return Err(bad_request("target must be 'agent' or 'user'".into()));
    }
    if req.query.len() > MAX_QUERY_BYTES {
        return Err(bad_request(format!(
            "query exceeds {MAX_QUERY_BYTES}-byte cap"
        )));
    }
    let limit = req.limit.unwrap_or(5).clamp(1, 20);
    let peer = req.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let arg = format!("{}|{}|{}|{}", req.subject_id, req.target, req.query, limit);
    let body = call_peer_string(&state, &peer, "memory.search", arg.as_bytes(), None).await?;
    let results = parse_search_body(&body);
    let count = results.len();
    Ok(Json(SearchResponse { results, count }))
}

pub async fn embed_all(
    State(state): State<AppState>,
    Json(req): Json<EmbedAllRequest>,
) -> Result<Json<EmbedAllResponse>, (StatusCode, Json<ApiError>)> {
    if req.subject_id.trim().is_empty() {
        return Err(bad_request("subject_id is required".into()));
    }
    let task_id = clean_optional_id(req.task_id.as_deref(), "task_id")?;
    let run_id = clean_optional(req.run_id.as_deref());
    let peer = req.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let detail = embed_all_detail(&req.subject_id);
    let body = match call_peer_string(
        &state,
        &peer,
        "memory.embed_all",
        req.subject_id.as_bytes(),
        task_id.as_deref(),
    )
    .await
    {
        Ok(body) => {
            record_embedding_activity(
                &state,
                EmbeddingActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "memory.embed_all",
                    decision: "ok",
                    detail: &detail,
                },
            );
            body
        }
        Err(err) => {
            record_embedding_activity(
                &state,
                EmbeddingActivity {
                    peer: &peer,
                    task_id: task_id.as_deref(),
                    run_id: run_id.as_deref(),
                    method: "memory.embed_all",
                    decision: "err",
                    detail: &detail,
                },
            );
            return Err(err);
        }
    };
    // Wire: `ok|chunks_embedded=N\n`.
    let trimmed = body.trim_end_matches('\n');
    let n: usize = trimmed
        .strip_prefix("ok|chunks_embedded=")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok(Json(EmbedAllResponse {
        ok: trimmed.starts_with("ok|"),
        chunks_embedded: n,
        task_id,
        run_id,
    }))
}

/// Parse the tab-separated `memory.search` body. Drops the
/// trailing `count=N\n` line; each data row is
/// `embedding_id\tscore\tchunk_text\n`.
pub fn parse_search_body(body: &str) -> Vec<SearchHit> {
    let mut out = Vec::new();
    for line in body.lines() {
        if line.starts_with("count=") {
            continue;
        }
        let cols: Vec<&str> = line.splitn(3, '\t').collect();
        if cols.len() != 3 {
            continue;
        }
        let Ok(score) = cols[1].parse::<f32>() else {
            continue;
        };
        out.push(SearchHit {
            embedding_id: cols[0].to_string(),
            score,
            chunk_text: cols[2].to_string(),
        });
    }
    out
}

fn bad_request(msg: String) -> (StatusCode, Json<ApiError>) {
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
        Err(bad_request(format!("{field} must be 32 hex chars")))
    }
}

fn embed_detail(subject_id: &str, target: &str, text_len: usize) -> String {
    format!("subject_id={subject_id}; target={target}; text_len={text_len}")
}

fn embed_all_detail(subject_id: &str) -> String {
    format!("subject_id={subject_id}")
}

struct EmbeddingActivity<'a> {
    peer: &'a str,
    task_id: Option<&'a str>,
    run_id: Option<&'a str>,
    method: &'a str,
    decision: &'a str,
    detail: &'a str,
}

fn record_embedding_activity(state: &AppState, activity: EmbeddingActivity<'_>) {
    let tenant_id = crate::tenant::current_tenant_or_none()
        .as_deref()
        .unwrap_or(DEFAULT_TENANT)
        .to_string();
    let actor = current_subject().unwrap_or_else(|| activity.method.to_string());
    if let Err(e) = append_tool_invocation_activity(
        state.cfg.transport.data_dir.as_deref(),
        ToolInvocationActivity {
            tenant_id: &tenant_id,
            actor: &actor,
            peer: activity.peer,
            method: activity.method,
            task_id: activity.task_id,
            run_id: activity.run_id,
            decision: activity.decision,
            detail: activity.detail,
        },
    ) {
        tracing::warn!(
            error = %e,
            method = activity.method,
            "failed to append memory embedding activity"
        );
    }
    if let (Some(rec), Some(task_id)) = (state.task_recorder.as_ref(), activity.task_id) {
        let payload = format!(
            "peer={} outcome={} {}",
            activity.peer, activity.decision, activity.detail
        );
        let rec = rec.clone();
        let task_id = task_id.to_string();
        let event_type = activity.method.to_string();
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
        ResponseResult::Err(env) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: format!("responder err kind={} cause={}", env.kind, env.cause),
            }),
        )),
        ResponseResult::StreamHandle(_) => Err((
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "unexpected stream response from memory peer".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_search_body_picks_data_rows_and_drops_count() {
        let body = "abc\t0.93\tthe quick brown fox\n\
                    def\t0.71\tlazy dog\n\
                    count=2\n";
        let v = parse_search_body(body);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].embedding_id, "abc");
        assert!((v[0].score - 0.93).abs() < 1e-5);
        assert_eq!(v[0].chunk_text, "the quick brown fox");
        assert_eq!(v[1].embedding_id, "def");
    }

    #[test]
    fn parse_search_body_skips_rows_with_unparseable_score() {
        let body = "abc\tNOPE\twhatever\ndef\t0.50\tok\ncount=1\n";
        let v = parse_search_body(body);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].embedding_id, "def");
    }

    #[test]
    fn parse_search_body_empty_returns_empty() {
        assert!(parse_search_body("count=0\n").is_empty());
        assert!(parse_search_body("").is_empty());
    }

    #[test]
    fn embed_request_accepts_task_and_run_context() {
        let req: EmbedRequest = serde_json::from_value(serde_json::json!({
            "subject_id": "user-1",
            "target": "user",
            "text": "hello",
            "peer": "memory-a",
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
    fn embed_response_serializes_scope_when_present() {
        let value = serde_json::to_value(EmbedResponse {
            embedding_id: "emb-1".into(),
            already_present: false,
            task_id: Some("0123456789abcdef0123456789abcdef".into()),
            run_id: Some("run-1".into()),
        })
        .unwrap();
        assert_eq!(value["task_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(value["run_id"], "run-1");
        assert!(value.get("already_present").is_none());
    }

    #[test]
    fn embed_all_response_serializes_scope_when_present() {
        let value = serde_json::to_value(EmbedAllResponse {
            ok: true,
            chunks_embedded: 7,
            task_id: Some("0123456789abcdef0123456789abcdef".into()),
            run_id: Some("run-1".into()),
        })
        .unwrap();
        assert_eq!(value["task_id"], "0123456789abcdef0123456789abcdef");
        assert_eq!(value["run_id"], "run-1");
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
        let err = clean_optional_id(Some("bad"), "task_id").unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.0.error, "task_id must be 32 hex chars");
    }

    #[test]
    fn embedding_activity_detail_does_not_copy_payload_text() {
        let secret = "do not log this literal document text";
        let detail = embed_detail("user-1", "user", secret.len());
        assert!(detail.contains("subject_id=user-1"));
        assert!(detail.contains("target=user"));
        assert!(detail.contains("text_len=37"));
        assert!(!detail.contains(secret));
    }
}
