//! PH-DASH-BLOCKLIST — HTTP proxy for `tool.web.blocklist_summary`.
//! Lets the dashboard show operators what they've configured in
//! `[tool] blocked_hosts` without reading the config file directly.
//!
//! One endpoint:
//!
//! - `GET /v1/tool/blocklist?peer=<alias>` — proxies
//!   `tool.web.blocklist_summary` to the named tool peer (default
//!   alias `"tool"`). Returns JSON:
//!
//!   ```json
//!   {"peer": "tool", "hosts": ["evil.example.com", ...], "count": 1}
//!   ```
//!
//! The list is sorted lexicographically by the responder, so the
//! bridge doesn't re-sort. Count comes from the responder's
//! `count=N` trailer line.
//!
//! Honest posture: this proxy is read-only; the dashboard cannot
//! mutate the blocklist. To change the blocklist, edit
//! `[tool] blocked_hosts` and restart the tool node.

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
pub struct BlocklistQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BlocklistResponse {
    pub peer: String,
    pub hosts: Vec<String>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub async fn blocklist(
    State(state): State<AppState>,
    Query(q): Query<BlocklistQuery>,
) -> Result<Json<BlocklistResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER).to_string();
    let body = call_peer(&state, &peer, "tool.web.blocklist_summary", b"").await?;
    let (hosts, count) = parse_body(&body);
    Ok(Json(BlocklistResponse { peer, hosts, count }))
}

/// PH-DASH-BLOCKLIST: parse the responder body. Line layout:
///
/// ```text
/// count=N
/// host-1
/// host-2
/// …
/// ```
///
/// We tolerate either ordering (count-first or count-last) since
/// it's cheap and defensive. If the count line is missing, we
/// fall back to the row count.
fn parse_body(body: &str) -> (Vec<String>, usize) {
    let mut hosts = Vec::new();
    let mut declared: Option<usize> = None;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("count=") {
            declared = rest.parse::<usize>().ok();
            continue;
        }
        hosts.push(trimmed.to_string());
    }
    let count = declared.unwrap_or(hosts.len());
    (hosts, count)
}

/// PH-DASH-BLOCKLIST: dial-and-call. Same error classification as
/// `fs_audit::call_peer` / `term_audit::call_peer` — kept local
/// (see PH-BRIDGE-DIAL-REFACTOR future cleanup).
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
                error: "unexpected stream response from tool.web.blocklist_summary".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_body_typical_count_first() {
        let body = "count=3\nalpha.example.com\nmiddle.example.com\nzebra.example.com\n";
        let (hosts, count) = parse_body(body);
        assert_eq!(count, 3);
        assert_eq!(
            hosts,
            vec![
                "alpha.example.com",
                "middle.example.com",
                "zebra.example.com"
            ]
        );
    }

    #[test]
    fn parse_body_empty_ring() {
        let body = "count=0\n";
        let (hosts, count) = parse_body(body);
        assert_eq!(count, 0);
        assert!(hosts.is_empty());
    }

    #[test]
    fn parse_body_completely_empty() {
        let (hosts, count) = parse_body("");
        assert_eq!(count, 0);
        assert!(hosts.is_empty());
    }

    #[test]
    fn parse_body_skips_blank_lines() {
        let body = "count=2\n\nalpha.example.com\n\n\nbeta.example.com\n";
        let (hosts, count) = parse_body(body);
        assert_eq!(count, 2);
        assert_eq!(hosts, vec!["alpha.example.com", "beta.example.com"]);
    }

    #[test]
    fn parse_body_count_missing_falls_back_to_row_count() {
        // Defensive: an older responder (or hand-crafted curl
        // response) without the trailer should still produce a
        // usable count.
        let body = "alpha.example.com\nbeta.example.com\n";
        let (hosts, count) = parse_body(body);
        assert_eq!(count, 2);
        assert_eq!(hosts, vec!["alpha.example.com", "beta.example.com"]);
    }

    #[test]
    fn parse_body_unparseable_count_falls_back_to_row_count() {
        let body = "count=notanumber\nalpha.example.com\n";
        let (hosts, count) = parse_body(body);
        assert_eq!(count, 1);
        assert_eq!(hosts, vec!["alpha.example.com"]);
    }

    #[test]
    fn parse_body_count_after_hosts_also_works() {
        // The responder lays out count-first, but the parser
        // shouldn't break if the order ever flips.
        let body = "alpha.example.com\nbeta.example.com\ncount=2\n";
        let (hosts, count) = parse_body(body);
        assert_eq!(count, 2);
        assert_eq!(hosts, vec!["alpha.example.com", "beta.example.com"]);
    }

    #[test]
    fn parse_body_trims_whitespace_on_host_lines() {
        let body = "count=1\n  whitespace.example.com  \n";
        let (hosts, count) = parse_body(body);
        assert_eq!(count, 1);
        assert_eq!(hosts, vec!["whitespace.example.com"]);
    }
}
