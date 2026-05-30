//! W2-006c — HTTP proxy for `node.dispatch.stats`. Lets the
//! dashboard show operators the per-capability invocation +
//! latency snapshot from any peer's DispatchBridge.
//!
//! One endpoint:
//!
//! - `GET /v1/dispatch/stats?peer=<alias>` — proxies
//!   `node.dispatch.stats` to the named peer (default alias
//!   `"tool"`). Returns JSON:
//!
//!   ```json
//!   {
//!     "peer": "tool",
//!     "rows": [
//!       {"method": "tool.web_fetch",
//!        "invocations": 12, "errors": 1, "denied": 0,
//!        "unknown_method": 0,
//!        "last_invoked_at": 1716, "last_error_at": 1715,
//!        "latency_samples": 13, "last_elapsed_ms": 240,
//!        "max_elapsed_ms": 1100, "mean_elapsed_ms": 312}
//!     ],
//!     "count": 1
//!   }
//!   ```
//!
//! Mean is computed server-side by the runtime; the bridge
//! just round-trips it.
//!
//! Honest scope: read-only. Counters are bridge-process lifetime
//! (reset on peer restart); no clearing endpoint.

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
pub struct DispatchStatsQuery {
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct DispatchStatsRow {
    pub method: String,
    pub invocations: u64,
    pub errors: u64,
    pub denied: u64,
    pub unknown_method: u64,
    pub last_invoked_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<i64>,
    pub latency_samples: u64,
    pub last_elapsed_ms: u64,
    pub max_elapsed_ms: u64,
    pub mean_elapsed_ms: u64,
    /// W2-006d: bounded ring of recent per-call latencies
    /// (oldest first). Empty when the responder doesn't ship
    /// the column (forward-compat with older peers).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub recent_latencies: Vec<u32>,
}

#[derive(Debug, Serialize)]
pub struct DispatchStatsResponse {
    pub peer: String,
    pub rows: Vec<DispatchStatsRow>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub async fn stats(
    State(state): State<AppState>,
    Query(q): Query<DispatchStatsQuery>,
) -> Result<Json<DispatchStatsResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER).to_string();
    let body = call_peer(&state, &peer, "node.dispatch.stats", b"").await?;
    let rows = parse_rows(&body);
    let count = rows.len();
    Ok(Json(DispatchStatsResponse { peer, rows, count }))
}

/// W2-006c: parse the tab-delim body emitted by
/// `controller_runtime::dispatch_stats_body`. Row layout:
/// `method\tinvocations\terrors\tdenied\tunknown_method\tlast_invoked_at\tlast_error_at\tlatency_samples\tlast_elapsed_ms\tmax_elapsed_ms\tmean_elapsed_ms`
/// followed by `count=N`.
fn parse_rows(body: &str) -> Vec<DispatchStatsRow> {
    let mut out = Vec::new();
    for line in body.lines() {
        if line.starts_with("count=") || line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 11 {
            continue;
        }
        let invocations = parts[1].parse().unwrap_or(0);
        let errors = parts[2].parse().unwrap_or(0);
        let denied = parts[3].parse().unwrap_or(0);
        let unknown_method = parts[4].parse().unwrap_or(0);
        let last_invoked_at = parts[5].parse().unwrap_or(0);
        let last_error_at = match parts[6] {
            "-" => None,
            s => s.parse::<i64>().ok(),
        };
        let latency_samples = parts[7].parse().unwrap_or(0);
        let last_elapsed_ms = parts[8].parse().unwrap_or(0);
        let max_elapsed_ms = parts[9].parse().unwrap_or(0);
        let mean_elapsed_ms = parts[10].parse().unwrap_or(0);
        // W2-006d: 12th column is the comma-separated recent
        // latencies ring (oldest-first). Empty / missing /
        // `-` all map to an empty Vec so an older peer
        // without the column still parses cleanly.
        let recent_latencies: Vec<u32> =
            if parts.len() >= 12 && parts[11] != "-" && !parts[11].is_empty() {
                parts[11]
                    .split(',')
                    .filter_map(|s| s.trim().parse::<u32>().ok())
                    .collect()
            } else {
                Vec::new()
            };
        out.push(DispatchStatsRow {
            method: parts[0].to_string(),
            invocations,
            errors,
            denied,
            unknown_method,
            last_invoked_at,
            last_error_at,
            latency_samples,
            last_elapsed_ms,
            max_elapsed_ms,
            mean_elapsed_ms,
            recent_latencies,
        });
    }
    out
}

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
                error: "unexpected stream response from node.dispatch.stats".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rows_typical_body() {
        let body = "node.health\t100\t2\t0\t0\t1716\t1700\t102\t5\t250\t12\n\
                    tool.web_fetch\t50\t3\t1\t0\t1716\t1715\t53\t240\t1100\t312\n\
                    count=2\n";
        let rows = parse_rows(body);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].method, "node.health");
        assert_eq!(rows[0].invocations, 100);
        assert_eq!(rows[0].errors, 2);
        assert_eq!(rows[0].last_error_at, Some(1700));
        assert_eq!(rows[0].mean_elapsed_ms, 12);
        assert_eq!(rows[1].method, "tool.web_fetch");
        assert_eq!(rows[1].max_elapsed_ms, 1100);
    }

    #[test]
    fn parse_rows_handles_no_error_dash() {
        let body = "node.health\t100\t0\t0\t0\t1716\t-\t100\t5\t10\t5\ncount=1\n";
        let rows = parse_rows(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].last_error_at, None);
    }

    #[test]
    fn parse_rows_drops_malformed() {
        let body = "broken\tonly-three\tcolumns\ncount=0\n";
        assert!(parse_rows(body).is_empty());
    }

    #[test]
    fn parse_rows_skips_count_and_blanks() {
        assert!(parse_rows("count=0\n").is_empty());
        assert!(parse_rows("\n\ncount=0\n").is_empty());
    }

    #[test]
    fn parse_rows_unparseable_numbers_default_to_zero() {
        // Defensive: a row with one bad numeric still parses;
        // the field defaults to 0 rather than dropping the
        // entire row.
        let body = "method\tnotanumber\t0\t0\t0\t1716\t-\t0\t0\t0\t0\ncount=1\n";
        let rows = parse_rows(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].invocations, 0);
    }

    #[test]
    fn row_skips_last_error_at_when_none() {
        let r = DispatchStatsRow {
            method: "m".into(),
            invocations: 0,
            errors: 0,
            denied: 0,
            unknown_method: 0,
            last_invoked_at: 0,
            last_error_at: None,
            latency_samples: 0,
            last_elapsed_ms: 0,
            max_elapsed_ms: 0,
            mean_elapsed_ms: 0,
            recent_latencies: Vec::new(),
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("last_error_at"));
        // W2-006d: empty recent_latencies serializes away.
        assert!(!s.contains("recent_latencies"));
    }

    #[test]
    fn parse_rows_reads_recent_latencies_csv() {
        let body = "node.health\t100\t2\t0\t0\t1716\t1700\t102\t5\t250\t12\t1,2,3,4,5\ncount=1\n";
        let rows = parse_rows(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].recent_latencies, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn parse_rows_treats_dash_as_empty_samples() {
        let body = "node.health\t1\t0\t0\t0\t1716\t-\t1\t10\t10\t10\t-\ncount=1\n";
        let rows = parse_rows(body);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].recent_latencies.is_empty());
    }

    #[test]
    fn parse_rows_forward_compat_without_samples_column() {
        // An older peer that doesn't ship column 12 still
        // parses; recent_latencies just stays empty.
        let body = "node.health\t1\t0\t0\t0\t1716\t-\t1\t10\t10\t10\ncount=1\n";
        let rows = parse_rows(body);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].recent_latencies.is_empty());
        // Other columns still parsed.
        assert_eq!(rows[0].max_elapsed_ms, 10);
    }

    #[test]
    fn parse_rows_drops_garbage_samples_keeps_good_ones() {
        let body = "node.health\t1\t0\t0\t0\t1716\t-\t1\t10\t10\t10\t1,bad,3, ,4\ncount=1\n";
        let rows = parse_rows(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].recent_latencies, vec![1, 3, 4]);
    }
}
