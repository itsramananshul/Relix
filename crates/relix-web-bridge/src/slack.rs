//! HTTP proxies for the slack channel surface.
//!
//! Two read-only endpoints — the bridge does not stand up a
//! slack client of its own; both handlers call the configured
//! slack peer and parse the pipe-/tab-delimited body the node
//! returns.
//!
//! - `GET /v1/slack/status` — bot online flag + identity + team
//!   + channel.
//! - `GET /v1/slack/messages/recent?limit=20` — last N inbound
//!   messages from the controller's bounded ring, newest-first.
//!
//! Mirrors `discord.rs`; the difference is the additional
//! `team_id` field on the status response.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
use relix_runtime::transport::envelope::ResponseResult;

use crate::config::AppState;

const DEFAULT_PEER: &str = "slack";

#[derive(Debug, Deserialize, Default)]
pub struct StatusQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct StatusResponse {
    pub peer: String,
    pub online: bool,
    pub username: String,
    pub user_id: String,
    pub team_id: String,
    pub channel_id: String,
    pub messages_seen: u64,
    pub last_message_at: Option<i64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct RecentQuery {
    #[serde(default)]
    pub peer: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct RecentMessage {
    /// Slack `ts` — string, not numeric.
    pub ts: String,
    pub user_id: String,
    pub username: String,
    pub channel_id: String,
    pub content_preview: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct RecentResponse {
    pub peer: String,
    pub messages: Vec<RecentMessage>,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub async fn status(
    State(state): State<AppState>,
    Query(q): Query<StatusQuery>,
) -> Result<Json<StatusResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let body = call_peer_string(&state, &peer, "slack.status", &[]).await?;
    let parsed = parse_status_body(&body).ok_or((
        StatusCode::BAD_GATEWAY,
        Json(ApiError {
            error: format!("slack.status returned an unparseable body: {body:?}"),
        }),
    ))?;
    Ok(Json(StatusResponse {
        peer,
        online: parsed.online,
        username: parsed.username,
        user_id: parsed.user_id,
        team_id: parsed.team_id,
        channel_id: parsed.channel_id,
        messages_seen: parsed.messages_seen,
        last_message_at: parsed.last_message_at,
    }))
}

pub async fn messages_recent(
    State(state): State<AppState>,
    Query(q): Query<RecentQuery>,
) -> Result<Json<RecentResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.unwrap_or_else(|| DEFAULT_PEER.to_string());
    let limit = q.limit.unwrap_or(20).clamp(1, 200);
    let body = call_peer_string(
        &state,
        &peer,
        "slack.messages_recent",
        limit.to_string().as_bytes(),
    )
    .await?;
    let messages = parse_recent_body(&body);
    Ok(Json(RecentResponse { peer, messages }))
}

#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct ParsedStatus {
    pub online: bool,
    pub username: String,
    pub user_id: String,
    pub team_id: String,
    pub channel_id: String,
    pub messages_seen: u64,
    pub last_message_at: Option<i64>,
}

pub fn parse_status_body(body: &str) -> Option<ParsedStatus> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = ParsedStatus::default();
    for kv in trimmed.split('|') {
        let (k, v) = kv.split_once('=')?;
        match k.trim() {
            "online" => out.online = v.trim() == "true",
            "username" => out.username = v.trim().to_string(),
            "user_id" => out.user_id = v.trim().to_string(),
            "team_id" => out.team_id = v.trim().to_string(),
            "channel_id" => out.channel_id = v.trim().to_string(),
            "messages_seen" => out.messages_seen = v.trim().parse().ok()?,
            "last_message_at" => {
                let n: i64 = v.trim().parse().ok()?;
                out.last_message_at = if n < 0 { None } else { Some(n) };
            }
            _ => {}
        }
    }
    Some(out)
}

pub fn parse_recent_body(body: &str) -> Vec<RecentMessage> {
    body.lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.splitn(5, '\t').collect();
            if cols.len() < 5 {
                return None;
            }
            Some(RecentMessage {
                ts: cols[0].to_string(),
                user_id: cols[1].to_string(),
                username: cols[2].to_string(),
                channel_id: cols[3].to_string(),
                content_preview: cols[4].to_string(),
            })
        })
        .collect()
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
                error: "unexpected stream response from slack peer".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_typical_online_body() {
        let body = "online=true|username=relixbot|user_id=U999|team_id=T999|channel_id=C01234567|messages_seen=3|last_message_at=1700000000\n";
        let p = parse_status_body(body).unwrap();
        assert!(p.online);
        assert_eq!(p.username, "relixbot");
        assert_eq!(p.user_id, "U999");
        assert_eq!(p.team_id, "T999");
        assert_eq!(p.channel_id, "C01234567");
        assert_eq!(p.messages_seen, 3);
        assert_eq!(p.last_message_at, Some(1700000000));
    }

    #[test]
    fn parse_status_offline_body_with_sentinel_timestamp() {
        let body = "online=false|username=|user_id=|team_id=|channel_id=C01234567|messages_seen=0|last_message_at=-1\n";
        let p = parse_status_body(body).unwrap();
        assert!(!p.online);
        assert!(p.team_id.is_empty());
        assert_eq!(p.last_message_at, None);
    }

    #[test]
    fn parse_status_empty_body_returns_none() {
        assert!(parse_status_body("").is_none());
        assert!(parse_status_body("   ").is_none());
    }

    #[test]
    fn parse_recent_typical_two_row_body() {
        let body = "1700000002.000100\tU2\tbob\tC0\thello\n1700000001.000100\tU1\talice\tC0\tfirst message\n";
        let v = parse_recent_body(body);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].ts, "1700000002.000100");
        assert_eq!(v[0].user_id, "U2");
        assert_eq!(v[0].username, "bob");
        assert_eq!(v[0].channel_id, "C0");
        assert_eq!(v[0].content_preview, "hello");
        assert_eq!(v[1].username, "alice");
    }

    #[test]
    fn parse_recent_skips_lines_with_too_few_columns() {
        let body = "200\tU2\tbob\nfull\trow\n";
        assert!(parse_recent_body(body).is_empty());
    }

    #[test]
    fn parse_recent_empty_body_returns_empty_vec() {
        assert!(parse_recent_body("").is_empty());
    }
}
