//! W2-007e — HTTP proxy for `node.policy.recent_denials`.
//! Surfaces the runtime-side denial ring (W2-007d) over a
//! single HTTP endpoint.
//!
//! One endpoint:
//!
//! - `GET /v1/policy/denials?peer=<alias>&max=<N>` —
//!   proxies `node.policy.recent_denials`. Returns JSON:
//!
//!   ```json
//!   {
//!     "peer": "tool",
//!     "denials": [
//!       {"at": 1716, "method": "tool.web_fetch",
//!        "caller_subject_id": "abcd...", "caller_name": "bob",
//!        "rule": "default_deny",
//!        "reason": "caller bob not admitted by [admit] groups"}
//!     ],
//!     "count": 1
//!   }
//!   ```
//!
//! Counts are lifetime per ring (bridge restart resets). The
//! audit log remains the canonical source — this is just a
//! fast operator view.

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
pub struct PolicyDenialsQuery {
    #[serde(default)]
    pub peer: Option<String>,
    /// Maximum entries returned. Defaults to 100; capped
    /// server-side by the runtime at 500.
    #[serde(default)]
    pub max: Option<usize>,
}

#[derive(Debug, Serialize, Clone, PartialEq, Eq)]
pub struct PolicyDenialRow {
    pub at: i64,
    pub method: String,
    pub caller_subject_id: String,
    pub caller_name: String,
    pub rule: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct PolicyDenialsResponse {
    pub peer: String,
    pub denials: Vec<PolicyDenialRow>,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub async fn denials(
    State(state): State<AppState>,
    Query(q): Query<PolicyDenialsQuery>,
) -> Result<Json<PolicyDenialsResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER).to_string();
    let arg = match q.max {
        Some(n) if n > 0 => n.to_string(),
        _ => String::new(),
    };
    let body = call_peer(&state, &peer, "node.policy.recent_denials", arg.as_bytes()).await?;
    let denials = parse_rows(&body);
    append_policy_activity(&state, &peer, &denials);
    let count = denials.len();
    Ok(Json(PolicyDenialsResponse {
        peer,
        denials,
        count,
    }))
}

fn append_policy_activity(state: &AppState, peer: &str, rows: &[PolicyDenialRow]) {
    let tenant_id = crate::tenant::current_tenant_or_none().unwrap_or_else(|| "default".into());
    for row in rows {
        let at_ms = row.at.saturating_mul(1000);
        if let Err(e) = crate::activity::append_policy_denial_activity(
            state.cfg.transport.data_dir.as_deref(),
            crate::activity::PolicyDenialActivity {
                tenant_id: &tenant_id,
                peer,
                at_ms,
                method: &row.method,
                caller_subject_id: &row.caller_subject_id,
                caller_name: &row.caller_name,
                rule: &row.rule,
                reason: &row.reason,
            },
        ) {
            tracing::warn!(
                method = row.method,
                rule = row.rule,
                error = %e,
                "policy denial accepted but activity ledger append failed"
            );
        }
    }
}

/// W2-007e: parse the tab-delim body emitted by
/// `controller_runtime::handle_policy_recent_denials`. Row
/// layout: `at\tmethod\tcaller_subject_id\tcaller_name\trule\treason`,
/// followed by `count=N`.
fn parse_rows(body: &str) -> Vec<PolicyDenialRow> {
    let mut out = Vec::new();
    for line in body.lines() {
        if line.starts_with("count=") || line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 6 {
            continue;
        }
        let at = match parts[0].parse::<i64>() {
            Ok(v) => v,
            Err(_) => continue,
        };
        out.push(PolicyDenialRow {
            at,
            method: parts[1].to_string(),
            caller_subject_id: parts[2].to_string(),
            caller_name: parts[3].to_string(),
            rule: parts[4].to_string(),
            reason: parts[5].to_string(),
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
                error: "unexpected stream response from node.policy.recent_denials".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rows_typical_body() {
        let body = "1716\ttool.web_fetch\tabcd\tbob\tdefault_deny\tno rule matched\n\
                    1717\ttool.terminal.run\tef01\talice\tcaller alice not in admit groups\tdenied\n\
                    count=2\n";
        let rows = parse_rows(body);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].method, "tool.web_fetch");
        assert_eq!(rows[0].caller_name, "bob");
        assert_eq!(rows[0].rule, "default_deny");
        assert_eq!(rows[1].caller_subject_id, "ef01");
    }

    #[test]
    fn parse_rows_drops_malformed() {
        // Missing columns + non-numeric `at` both dropped.
        let body = "broken\tcols\n\
                    notanumber\tm\tx\ty\tr\treason\n\
                    100\tm\tx\ty\tr\treason\n\
                    count=3\n";
        let rows = parse_rows(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].at, 100);
    }

    #[test]
    fn parse_rows_skips_count_and_blanks() {
        assert!(parse_rows("count=0\n").is_empty());
        assert!(parse_rows("\ncount=0\n").is_empty());
    }

    #[test]
    fn parse_rows_handles_extra_columns_gracefully() {
        // Forward-compat: runtime grows a 7th column → bridge
        // still picks the first 6 cleanly.
        let body = "100\tm\tx\ty\tr\treason\tfuture\ncount=1\n";
        let rows = parse_rows(body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reason, "reason");
    }
}
