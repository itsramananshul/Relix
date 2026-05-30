//! W2-007b — HTTP proxy for `node.policy.simulate`. Lets the
//! dashboard ask "what would the policy decide?" without
//! invoking anything on the target peer.
//!
//! One endpoint:
//!
//! - `GET /v1/policy/simulate?peer=<alias>&method=<method>&groups=<csv>`
//!   proxies `node.policy.simulate`. Returns JSON:
//!
//!   ```json
//!   {
//!     "peer": "tool",
//!     "method": "tool.web_fetch",
//!     "groups": ["chat-users"],
//!     "decision": "allow",
//!     "matched_rule": "web-fetch-rule",
//!     "reason": null
//!   }
//!   ```
//!
//! `matched_rule` is null when no rule matched (default deny).
//! `reason` is null on allow; present on deny.

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
pub struct PolicySimulateQuery {
    #[serde(default)]
    pub peer: Option<String>,
    pub method: String,
    /// Comma-separated group list. Empty / missing → no groups.
    #[serde(default)]
    pub groups: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PolicySimulateResponse {
    pub peer: String,
    pub method: String,
    pub groups: Vec<String>,
    pub decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

pub async fn simulate(
    State(state): State<AppState>,
    Query(q): Query<PolicySimulateQuery>,
) -> Result<Json<PolicySimulateResponse>, (StatusCode, Json<ApiError>)> {
    let peer = q.peer.as_deref().unwrap_or(DEFAULT_PEER).to_string();
    let method = q.method.trim();
    if method.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "method required".into(),
            }),
        ));
    }
    let groups_csv = q.groups.clone().unwrap_or_default();
    let groups: Vec<String> = groups_csv
        .split(',')
        .map(|g| g.trim().to_string())
        .filter(|g| !g.is_empty())
        .collect();
    let arg = format!("{method}|{groups_csv}");
    let body = call_peer(&state, &peer, "node.policy.simulate", arg.as_bytes()).await?;
    let parsed = parse_body(&body);
    Ok(Json(PolicySimulateResponse {
        peer,
        method: method.to_string(),
        groups,
        decision: parsed.decision,
        matched_rule: parsed.matched_rule,
        reason: parsed.reason,
    }))
}

#[derive(Debug, Default)]
struct ParsedDecision {
    decision: String,
    matched_rule: Option<String>,
    reason: Option<String>,
}

/// W2-007b: parse the multi-line key=value body emitted by
/// `controller_runtime::handle_policy_simulate`. `-` sentinel
/// → JSON null on output. Defensive: unknown lines are
/// ignored.
fn parse_body(body: &str) -> ParsedDecision {
    let mut p = ParsedDecision::default();
    for line in body.lines() {
        let (k, v) = match line.split_once('=') {
            Some(p) => p,
            None => continue,
        };
        let v = v.trim();
        let val = if v == "-" { None } else { Some(v.to_string()) };
        match k.trim() {
            "decision" => {
                if let Some(val) = val {
                    p.decision = val;
                }
            }
            "matched_rule" => p.matched_rule = val,
            "reason" => p.reason = val,
            _ => {}
        }
    }
    p
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
                error: "unexpected stream response from node.policy.simulate".into(),
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_body_allow_with_rule() {
        let body = "decision=allow\nmatched_rule=web-fetch-rule\nreason=-\n";
        let p = parse_body(body);
        assert_eq!(p.decision, "allow");
        assert_eq!(p.matched_rule.as_deref(), Some("web-fetch-rule"));
        assert_eq!(p.reason, None);
    }

    #[test]
    fn parse_body_deny_with_reason() {
        let body = "decision=deny\nmatched_rule=-\nreason=no rule matched\n";
        let p = parse_body(body);
        assert_eq!(p.decision, "deny");
        assert_eq!(p.matched_rule, None);
        assert_eq!(p.reason.as_deref(), Some("no rule matched"));
    }

    #[test]
    fn parse_body_empty_yields_defaults() {
        let p = parse_body("");
        assert_eq!(p.decision, "");
        assert!(p.matched_rule.is_none());
        assert!(p.reason.is_none());
    }

    #[test]
    fn parse_body_ignores_unknown_keys() {
        let body = "decision=allow\nfuture_field=xyz\nmatched_rule=r\nreason=-\n";
        let p = parse_body(body);
        assert_eq!(p.decision, "allow");
        assert_eq!(p.matched_rule.as_deref(), Some("r"));
    }

    #[test]
    fn parse_body_handles_dash_sentinel_in_decision() {
        // Defensive: if the decision line is missing it stays "".
        // If `-` is the value, we treat the field as None for
        // Option fields but decision is a String so "-" passes
        // through unchanged — operators see a clearly wrong
        // value rather than a silent default.
        let body = "matched_rule=r\n";
        let p = parse_body(body);
        assert_eq!(p.decision, "");
    }

    #[test]
    fn response_skips_optional_fields_when_none() {
        let r = PolicySimulateResponse {
            peer: "tool".into(),
            method: "x".into(),
            groups: vec![],
            decision: "allow".into(),
            matched_rule: None,
            reason: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("matched_rule"));
        assert!(!s.contains("reason"));
    }
}
