//! W2-MEMORY-CURATOR-3 — HTTP proxies for the memory curator
//! surface.
//!
//! Two endpoints:
//!
//! - `POST /v1/memory/curate` — operator-triggered curation
//!   for one subject_id. Proxies `memory.agent_curate` on the
//!   memory node and returns the parsed pipe-delimited
//!   summary as JSON.
//!
//! - `GET /v1/memory/curator/status` — read-only view of the
//!   scheduler's last-run timing + summary. Proxies a new
//!   memory-node capability (today: synthesized by the bridge
//!   from the curate response since the runtime doesn't yet
//!   expose `memory.curator_status` — that lands as a follow-
//!   up). For now, the status endpoint returns 503 and a
//!   clear "scheduler status not yet readable from the bridge"
//!   message; manual curation through the same surface works
//!   end-to-end.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "memory";
const DEFAULT_AI_PEER: &str = "ai";

/// POST `/v1/memory/curate` body. `subject_id` is required —
/// validated inside the handler so a missing field returns
/// `400` (with our `ApiError` JSON shape) rather than axum's
/// default 422 for JSON deserialization failures.
#[derive(Debug, Deserialize, Default)]
pub struct CurateRequest {
    /// The agent's 64-char hex subject_id.
    #[serde(default)]
    pub subject_id: Option<String>,
    /// Memory peer alias. Defaults to `"memory"`.
    #[serde(default)]
    pub peer: Option<String>,
    /// AI peer alias used by the memory node for this
    /// curation pass. Defaults to `"ai"`. The memory node
    /// configures the actual peer address; this alias is
    /// informational today (forward-compat for multi-AI
    /// routing).
    #[serde(default)]
    pub ai_peer: Option<String>,
}

/// Parsed curation summary (one entry per field in the
/// memory-node wire body).
#[derive(Debug, Serialize, Default, Clone, PartialEq, Eq)]
pub struct CurateSummary {
    pub agent_entries_before: usize,
    pub agent_entries_after: usize,
    pub agent_chars_before: usize,
    pub agent_chars_after: usize,
    pub user_entries_before: usize,
    pub user_entries_after: usize,
    pub user_chars_before: usize,
    pub user_chars_after: usize,
    pub chars_saved: usize,
}

#[derive(Debug, Serialize)]
pub struct CurateResponse {
    pub peer: String,
    pub subject_id: String,
    pub result: CurateSummary,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub async fn curate(
    State(state): State<AppState>,
    Json(req): Json<CurateRequest>,
) -> Result<Json<CurateResponse>, (StatusCode, Json<ApiError>)> {
    let subject_id = req.subject_id.as_deref().unwrap_or("").trim().to_string();
    if subject_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "subject_id required".into(),
            }),
        ));
    }
    let peer = req.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let ai_peer = req.ai_peer.unwrap_or_else(|| DEFAULT_AI_PEER.to_string());
    let arg = format!("{subject_id}|{ai_peer}");
    let body = call_peer_string(&state, &peer, "memory.agent_curate", arg.as_bytes()).await?;
    let summary = parse_curate_body(&body).ok_or((
        StatusCode::BAD_GATEWAY,
        Json(ApiError {
            error: format!(
                "memory peer returned unparseable agent_curate body ({} chars)",
                body.len()
            ),
        }),
    ))?;
    Ok(Json(CurateResponse {
        peer,
        subject_id,
        result: summary,
    }))
}

#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    /// Memory peer the bridge proxied the request to.
    pub peer: String,
    /// Honest scope note: the runtime doesn't yet expose a
    /// `memory.curator_status` capability — the scheduler's
    /// state lives in-process on the memory node. This
    /// endpoint surfaces what the bridge knows
    /// (enabled-via-config) and an explicit `bridge_note`
    /// field naming the gap.
    pub enabled: Option<bool>,
    pub interval_secs: Option<u64>,
    pub bridge_note: String,
}

pub async fn status(
    State(_state): State<AppState>,
    Query(q): Query<StatusQuery>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    // The memory node doesn't yet expose a status capability
    // — the in-process `CuratorState` is only readable by the
    // node itself. We could ship a `memory.curator_status`
    // capability that reads the state and returns it, but
    // that's a separate follow-up. For now the bridge
    // surface tells the operator what's missing rather than
    // making something up.
    Ok(Json(StatusResponse {
        peer,
        enabled: None,
        interval_secs: None,
        bridge_note:
            "scheduler status is in-process on the memory node; a memory.curator_status capability that exposes last_run_at / last_run_summary / next_run_at lands in a follow-up. Trigger curation manually via POST /v1/memory/curate."
                .into(),
    }))
}

/// Parse the pipe-delimited body emitted by
/// `memory.agent_curate`. Returns `None` on any malformed
/// input. Tolerant of trailing whitespace.
pub fn parse_curate_body(body: &str) -> Option<CurateSummary> {
    let line = body.trim();
    if line.is_empty() {
        return None;
    }
    let mut out = CurateSummary::default();
    // Numeric fields require a usize parse; non-numeric fields
    // (subject_id, future model-name fields, etc.) pass through
    // untouched. Bad numeric value on a known field is treated
    // as a malformed body — the bridge prefers a clean 502 over
    // a silently-zeroed summary.
    for field in line.split('|') {
        let (k, v) = field.split_once('=')?;
        match k {
            "agent_entries_before" => out.agent_entries_before = v.parse().ok()?,
            "agent_entries_after" => out.agent_entries_after = v.parse().ok()?,
            "agent_chars_before" => out.agent_chars_before = v.parse().ok()?,
            "agent_chars_after" => out.agent_chars_after = v.parse().ok()?,
            "user_entries_before" => out.user_entries_before = v.parse().ok()?,
            "user_entries_after" => out.user_entries_after = v.parse().ok()?,
            "user_chars_before" => out.user_chars_before = v.parse().ok()?,
            "user_chars_after" => out.user_chars_after = v.parse().ok()?,
            "chars_saved" => out.chars_saved = v.parse().ok()?,
            _ => {} // subject_id, forward-compat fields ignored
        }
    }
    Some(out)
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
            error: "bridge mesh client not initialized (peer discovery failed at startup)".into(),
        }),
    ))?;
    // Bigger deadline than the default for curate — the
    // memory peer is calling out to the AI peer, which is
    // slow. Cap at 120s.
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(60, 120);
    let envelope = build_request(
        method,
        arg.to_vec(),
        state.identity_bundle.clone(),
        deadline_secs,
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
                error: "unexpected stream response from memory.agent_curate".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_typical_curate_body() {
        let body = "subject_id=alice|agent_entries_before=5|agent_entries_after=3|agent_chars_before=200|agent_chars_after=120|user_entries_before=3|user_entries_after=2|user_chars_before=80|user_chars_after=50|chars_saved=110\n";
        let s = parse_curate_body(body).unwrap();
        assert_eq!(s.agent_entries_before, 5);
        assert_eq!(s.agent_entries_after, 3);
        assert_eq!(s.agent_chars_before, 200);
        assert_eq!(s.agent_chars_after, 120);
        assert_eq!(s.user_entries_before, 3);
        assert_eq!(s.user_entries_after, 2);
        assert_eq!(s.user_chars_before, 80);
        assert_eq!(s.user_chars_after, 50);
        assert_eq!(s.chars_saved, 110);
    }

    #[test]
    fn parse_empty_body_returns_none() {
        assert!(parse_curate_body("").is_none());
        assert!(parse_curate_body("   ").is_none());
    }

    #[test]
    fn parse_rejects_malformed_field() {
        let body = "agent_chars_before=NaN|user_chars_after=10";
        assert!(parse_curate_body(body).is_none());
    }

    #[test]
    fn parse_tolerates_extra_unknown_field_forward_compat() {
        // A future memory-node version that adds a `model=...`
        // field should still parse cleanly.
        let body = "subject_id=alice|agent_entries_before=1|agent_entries_after=1|agent_chars_before=10|agent_chars_after=10|user_entries_before=0|user_entries_after=0|user_chars_before=0|user_chars_after=0|chars_saved=0|model=gpt-99\n";
        let s = parse_curate_body(body).unwrap();
        assert_eq!(s.agent_chars_before, 10);
    }
}
