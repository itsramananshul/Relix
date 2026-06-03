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
    http::{header, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

/// The coordinator's mesh alias (same as the `agent.*` routes).
const DEFAULT_PEER: &str = "coordinator";

/// The self-contained spine board page (inline HTML/JS/CSS, no
/// bundler / CDN — baked into the binary, same convention as
/// `/dashboard`). It fetches the `/v1/spine/*` routes below.
const SPINE_HTML: &str = include_str!("spine_dashboard.html");

/// Per-route CSP allowing the page's inline `<script>`/`<style>`,
/// same as `/dashboard`. `connect-src 'self'` lets it call the
/// same-origin `/v1/spine/*` API; every other route keeps the
/// strict default CSP.
const SPINE_CSP: &str = "default-src 'self'; \
                         script-src 'self' 'unsafe-inline'; \
                         style-src 'self' 'unsafe-inline'; \
                         img-src 'self' data:; \
                         connect-src 'self'";

/// `GET /spine` — the product-spine board page.
pub async fn page() -> Response {
    let xcto = HeaderName::from_static("x-content-type-options");
    match Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CACHE_CONTROL, "public, max-age=300")
        .header(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(SPINE_CSP),
        )
        .header(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"))
        .header(xcto, HeaderValue::from_static("nosniff"))
        .header(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        )
        .body(SPINE_HTML.to_string())
    {
        Ok(r) => r.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "spine page builder failed",
        )
            .into_response(),
    }
}

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

/// `GET /v1/spine/mandates/:id/tree` — a Mandate with its direct
/// sub-Mandates and Campaigns.
pub async fn mandate_tree(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(call_peer(&state, "mandate.tree", id.as_bytes()).await?)
}

/// `GET /v1/spine/mandates/:id/briefs` — the Briefs under a Mandate.
pub async fn mandate_briefs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{}|{}", id, q.limit.unwrap_or(100));
    json_passthrough(call_peer(&state, "mandate.briefs", arg.as_bytes()).await?)
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

// ── write routes ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateBriefRequest {
    pub title: String,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub mandate: Option<String>,
    #[serde(default)]
    pub campaign: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
}

/// `POST /v1/spine/briefs` — materialize a Brief. Returns the id.
pub async fn create_brief(
    State(state): State<AppState>,
    Json(req): Json<CreateBriefRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if req.title.trim().is_empty() {
        return Err(bad("title required"));
    }
    let opt = |o: &Option<String>| o.clone().unwrap_or_default();
    // The wire arg is pipe-delimited; none of these positional fields
    // may contain a literal `|` or they'd shift the arg layout.
    for (label, val) in [
        ("title", req.title.as_str()),
        ("assignee", &opt(&req.assignee)),
        ("mandate", &opt(&req.mandate)),
        ("campaign", &opt(&req.campaign)),
        ("priority", &opt(&req.priority)),
    ] {
        if val.contains('|') {
            return Err(bad(&format!("{label} must not contain `|`")));
        }
    }
    let arg = format!(
        "{}|{}|{}|{}|{}",
        req.title,
        opt(&req.assignee),
        opt(&req.mandate),
        opt(&req.campaign),
        opt(&req.priority)
    );
    let body = call_peer(&state, "brief.create", arg.as_bytes()).await?;
    json_id("task_id", &body)
}

#[derive(Debug, Deserialize)]
pub struct MoveRequest {
    pub status: String,
}

/// `POST /v1/spine/briefs/:id/move` — move a Brief on the board.
pub async fn move_brief(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<MoveRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{id}|{}", req.status);
    call_peer(&state, "brief.move", arg.as_bytes()).await?;
    ok_json()
}

#[derive(Debug, Deserialize, Default)]
pub struct PinRequest {
    #[serde(default)]
    pub pinned: bool,
}

/// `POST /v1/spine/briefs/:id/pin` — pin/unpin a Brief.
pub async fn pin_brief(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<PinRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = format!("{id}|{}", i32::from(req.pinned));
    call_peer(&state, "brief.pin", arg.as_bytes()).await?;
    ok_json()
}

#[derive(Debug, Deserialize)]
pub struct CommentRequest {
    pub author: String,
    pub text: String,
}

/// `POST /v1/spine/briefs/:id/comment` — comment on a Brief.
pub async fn comment_brief(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<CommentRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if req.author.trim().is_empty() || req.text.trim().is_empty() {
        return Err(bad("author and text required"));
    }
    if req.author.contains('|') {
        return Err(bad("author must not contain `|`"));
    }
    // `text` is the trailing field (splitn 3) so it may contain `|`.
    let arg = format!("{id}|{}|{}", req.author, req.text);
    call_peer(&state, "brief.comment", arg.as_bytes()).await?;
    ok_json()
}

#[derive(Debug, Deserialize, Default)]
pub struct DueRequest {
    /// Unix seconds; null/omitted clears the due date.
    #[serde(default)]
    pub due_at: Option<i64>,
}

/// `POST /v1/spine/briefs/:id/due` — set/clear a Brief's due date.
pub async fn set_due(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<DueRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let arg = match req.due_at {
        Some(v) => format!("{id}|{v}"),
        None => format!("{id}|"),
    };
    call_peer(&state, "brief.set_due", arg.as_bytes()).await?;
    ok_json()
}

#[derive(Debug, Deserialize)]
pub struct CreateMandateRequest {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub owner_agent_id: Option<String>,
    #[serde(default)]
    pub parent_mandate_id: Option<String>,
}

/// `POST /v1/spine/mandates` — create a Mandate. Returns the id.
pub async fn create_mandate(
    State(state): State<AppState>,
    Json(req): Json<CreateMandateRequest>,
) -> Result<Response, (StatusCode, Json<ApiError>)> {
    if req.title.trim().is_empty() {
        return Err(bad("title required"));
    }
    let opt = |o: &Option<String>| o.clone().unwrap_or_default();
    for (label, val) in [
        ("title", req.title.as_str()),
        ("description", &opt(&req.description)),
        ("owner_agent_id", &opt(&req.owner_agent_id)),
        ("parent_mandate_id", &opt(&req.parent_mandate_id)),
    ] {
        if val.contains('|') {
            return Err(bad(&format!("{label} must not contain `|`")));
        }
    }
    let arg = format!(
        "{}|{}|{}|{}",
        req.title,
        opt(&req.description),
        opt(&req.owner_agent_id),
        opt(&req.parent_mandate_id)
    );
    let body = call_peer(&state, "mandate.create", arg.as_bytes()).await?;
    json_id("mandate_id", &body)
}

// ── helpers ───────────────────────────────────────────────

/// A `200 {"ok":true}` for write actions with no return value.
fn ok_json() -> Result<Response, (StatusCode, Json<ApiError>)> {
    json_passthrough(br#"{"ok":true}"#.to_vec())
}

/// Wrap a raw id body as `{"<field>":"<id>"}`.
fn json_id(field: &str, body: &[u8]) -> Result<Response, (StatusCode, Json<ApiError>)> {
    let id = String::from_utf8_lossy(body);
    let payload = serde_json::json!({ field: id.trim() }).to_string();
    json_passthrough(payload.into_bytes())
}

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

    #[tokio::test]
    async fn spine_page_is_html_with_inline_csp() {
        let resp = page().await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ctype = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ctype.starts_with("text/html"), "ctype was {ctype:?}");
        let csp = resp
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(csp.contains("script-src 'self' 'unsafe-inline'"), "csp: {csp:?}");
        assert!(csp.contains("connect-src 'self'"), "csp: {csp:?}");
    }

    #[test]
    fn spine_page_html_references_the_api() {
        // The baked page must actually call the spine API it depends
        // on — a cheap guard against the HTML drifting from the routes.
        assert!(SPINE_HTML.contains("/v1/spine/board"));
        assert!(SPINE_HTML.contains("/v1/spine/briefs"));
        assert!(SPINE_HTML.contains("/v1/spine/guild"));
        assert!(SPINE_HTML.contains("/v1/spine/mandates"));
    }

    /// Build the full spine route table in isolation: matchit panics
    /// at `.route()` time on an overlapping/ambiguous pattern, so a
    /// clean construction here proves the routes are valid (the full
    /// app router is only built at server startup, not in tests).
    #[test]
    fn spine_routes_construct_without_conflict() {
        use axum::routing::{get, post};
        let _router: axum::Router<crate::config::AppState> = axum::Router::new()
            .route("/spine", get(page))
            .route("/v1/spine/guild", get(guild_counts))
            .route("/v1/spine/board", get(board_summary))
            .route("/v1/spine/board/:column", get(board_column))
            .route("/v1/spine/roster", get(roster_summary))
            .route(
                "/v1/spine/mandates",
                get(mandates).post(create_mandate),
            )
            .route("/v1/spine/mandates/search", get(mandate_search))
            .route("/v1/spine/mandates/:id/tree", get(mandate_tree))
            .route("/v1/spine/mandates/:id/briefs", get(mandate_briefs))
            .route("/v1/spine/briefs/search", get(brief_search))
            .route("/v1/spine/briefs/:id", get(brief_detail))
            .route("/v1/spine/desk/:agent", get(desk))
            .route("/v1/spine/overdue", get(overdue))
            .route("/v1/spine/briefs", post(create_brief))
            .route("/v1/spine/briefs/:id/move", post(move_brief))
            .route("/v1/spine/briefs/:id/pin", post(pin_brief))
            .route("/v1/spine/briefs/:id/comment", post(comment_brief))
            .route("/v1/spine/briefs/:id/due", post(set_due));
    }
}
