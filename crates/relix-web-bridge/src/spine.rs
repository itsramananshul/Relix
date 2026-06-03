//! Product-spine HTTP surface for the dashboard.
//!
//! Thin read proxies that dial the coordinator and forward the
//! product-spine capabilities (`brief.*` / `mandate.*` / `agent.*`
//! summaries) to the browser as JSON. Every call goes through the
//! mesh admission pipeline via the bridge identity, exactly like the
//! `agent.*` / `task.*` routes — these add no new trust, just a
//! browser-friendly shape over the existing capabilities.

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::Response,
    Json,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

/// The coordinator's mesh alias (same as the `agent.*` routes).
const DEFAULT_PEER: &str = "coordinator";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// Optional status filter for `mandate.list`.
    #[serde(default)]
    pub status: Option<String>,
    /// Optional limit (search / list / overdue).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional free-text query for the search routes.
    #[serde(default)]
    pub q: Option<String>,
}

// ── routes ────────────────────────────────────────────────

/// `GET /v1/spine/guild` — the Guild's Mandate/Campaign rollup.
pub async fn guild_counts(
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "guild.counts", b"").await?)
}

/// `GET /v1/spine/board` — Brief counts by board column.
pub async fn board_summary(
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "brief.board_summary", b"").await?)
}

/// `GET /v1/spine/roster` — Operative counts by status.
pub async fn roster_summary(
    State(state): State<AppState>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "agent.roster_summary", b"").await?)
}

/// `GET /v1/spine/mandates?status=` — Mandates (optionally filtered).
pub async fn mandates(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = q.status.unwrap_or_default();
    json_passthrough(call_peer(&state, "mandate.list", arg.as_bytes()).await?)
}

/// `GET /v1/spine/mandates/search?q=&limit=` — Mandate title search.
pub async fn mandate_search(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let query = q.q.unwrap_or_default();
    if query.trim().is_empty() {
        return Err(bad("q (query) required"));
    }
    let arg = format!("{}|{}", query, q.limit.unwrap_or(50));
    json_passthrough(call_peer(&state, "mandate.search", arg.as_bytes()).await?)
}

/// `GET /v1/spine/briefs/:id` — the full Brief detail view.
pub async fn brief_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "brief.detail", id.as_bytes()).await?)
}

/// `GET /v1/spine/board/:column?limit=` — the Briefs in one column.
pub async fn board_column(
    State(state): State<AppState>,
    Path(column): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{}|{}", column, q.limit.unwrap_or(50));
    json_passthrough(call_peer(&state, "brief.board", arg.as_bytes()).await?)
}

/// `GET /v1/spine/desk/:agent?limit=` — an Operative's in-flight Briefs.
pub async fn desk(
    State(state): State<AppState>,
    Path(agent): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{}|{}", agent, q.limit.unwrap_or(50));
    json_passthrough(call_peer(&state, "brief.desk", arg.as_bytes()).await?)
}

/// `GET /v1/spine/overdue?limit=` — the overdue Briefs.
pub async fn overdue(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("|{}", q.limit.unwrap_or(50));
    json_passthrough(call_peer(&state, "brief.overdue", arg.as_bytes()).await?)
}

/// `GET /v1/spine/briefs/search?q=&limit=` — Brief title search.
pub async fn brief_search(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let query = q.q.unwrap_or_default();
    if query.trim().is_empty() {
        return Err(bad("q (query) required"));
    }
    let arg = format!("{}|{}", query, q.limit.unwrap_or(50));
    json_passthrough(call_peer(&state, "brief.search", arg.as_bytes()).await?)
}

// ── helpers ───────────────────────────────────────────────

fn bad(msg: &str) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError { error: msg.into() }),
    )
}

/// Wrap a raw mesh body (already JSON for these capabilities) in a
/// `200 application/json` response. An empty body becomes `null`.
fn json_passthrough(body: Vec<u8>) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let payload = if body.is_empty() { b"null".to_vec() } else { body };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(payload))
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError {
                    error: format!("build response: {e}"),
                }),
            )
        })
}

/// Dial the coordinator and invoke `method` with `arg`, returning
/// the raw response body. Mirrors the `agent.*` routes' helper.
async fn call_peer(
    state: &AppState,
    method: &str,
    arg: &[u8],
) -> Result<Vec<u8>, (StatusCode, Json<ApiError>)> {
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
    let resp_bytes = mesh.call(DEFAULT_PEER, envelope).await.map_err(|e| {
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
    fn json_passthrough_wraps_body_and_nulls_empty() {
        // Non-empty JSON body passes through with a JSON content type.
        let resp = json_passthrough(br#"{"total":3}"#.to_vec()).unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );

        // An empty mesh body (e.g. "no labels") becomes JSON null, not
        // an empty 200 the browser can't parse.
        let empty = json_passthrough(Vec::new()).unwrap();
        assert_eq!(empty.status(), StatusCode::OK);
    }

    #[test]
    fn bad_is_a_400() {
        let (status, body) = bad("q required");
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body.0.error, "q required");
    }
}
