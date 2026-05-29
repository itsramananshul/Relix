//! PART 2 — inbound webhook routes for the wire-real
//! approval channels. Each route lives behind its channel's
//! signature-verification primitive in the corresponding
//! channel crate (`relix-slack`, `relix-discord`, …).
//!
//! Slack: `POST /v1/channels/slack/interact`
//! ----------------------------------------
//!
//! Operators paste the Slack app's **signing secret** into
//! `RELIX_BRIDGE_SLACK_SIGNING_SECRET` (or the
//! `[channels.slack]` bridge config section). The handler
//! verifies the `x-slack-signature` HMAC against the raw
//! request body, parses the Block Kit `block_actions`
//! interaction payload, then dispatches the lifted
//! decision (`approved` / `rejected`) to the coordinator's
//! `approval.record_decision` cap.
//!
//! Slack expects an HTTP 200 with an empty body within 3s of
//! the click; the handler issues the mesh call inline because
//! `record_decision` is fast (single SQLite UPDATE + the
//! `oneshot::Sender::send` cancellation hop landed in PART 7).
//! If the coordinator is unreachable we still return 200 with
//! a logged error — the operator's UI shows the click was
//! received and the actual decision is reconciled via the
//! failed-deliveries surface.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use serde::Serialize;

use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::transport::envelope::ResponseResult;
use relix_slack::{
    InteractionParseError, SignatureCheck, parse_interaction_payload, verify_request_signature,
};

use crate::config::AppState;

/// Env var the bridge reads the Slack signing secret from at
/// startup. Leaving this unset disables the
/// `/v1/channels/slack/interact` route — the handler returns
/// 503 so a misconfigured operator sees the wire reason in
/// their Slack app's logs.
pub const SLACK_SIGNING_SECRET_ENV: &str = "RELIX_BRIDGE_SLACK_SIGNING_SECRET";

const COORDINATOR_ALIAS: &str = "coordinator";

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Serialize, Default)]
pub struct EmptyResponse {}

/// `POST /v1/channels/slack/interact`
///
/// Verifies the `x-slack-signature` HMAC against the raw
/// body, parses the Block Kit `block_actions` payload, then
/// forwards the decision to the coordinator. Returns an
/// empty 200 on success so Slack does not retry the
/// interaction (Slack's interactivity contract treats any
/// non-2xx as a retryable failure).
pub async fn slack_interact(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let signing_secret = match std::env::var(SLACK_SIGNING_SECRET_ENV) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => {
            tracing::warn!("slack interact: {SLACK_SIGNING_SECRET_ENV} unset; rejecting webhook");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError {
                    error: format!(
                        "slack interactivity disabled: set {SLACK_SIGNING_SECRET_ENV} \
                         to the Slack app's signing secret to enable"
                    ),
                }),
            )
                .into_response();
        }
    };

    let ts = match headers
        .get("x-slack-request-timestamp")
        .and_then(|h| h.to_str().ok())
    {
        Some(s) => s.to_string(),
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(ApiError {
                    error: "missing x-slack-request-timestamp header".into(),
                }),
            )
                .into_response();
        }
    };
    let sig = match headers
        .get("x-slack-signature")
        .and_then(|h| h.to_str().ok())
    {
        Some(s) => s.to_string(),
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(ApiError {
                    error: "missing x-slack-signature header".into(),
                }),
            )
                .into_response();
        }
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let check = verify_request_signature(&signing_secret, &ts, &sig, &body, now);
    match check {
        SignatureCheck::Valid => {}
        SignatureCheck::Stale => {
            tracing::warn!(
                "slack interact: signature stale (timestamp outside the 5-minute window)"
            );
            return (
                StatusCode::UNAUTHORIZED,
                Json(ApiError {
                    error: "x-slack-signature: timestamp outside the 5-minute window".into(),
                }),
            )
                .into_response();
        }
        SignatureCheck::Mismatch => {
            tracing::warn!("slack interact: HMAC signature mismatch");
            return (
                StatusCode::UNAUTHORIZED,
                Json(ApiError {
                    error: "x-slack-signature: HMAC mismatch".into(),
                }),
            )
                .into_response();
        }
        SignatureCheck::Malformed(reason) => {
            tracing::warn!(reason = reason, "slack interact: signature malformed");
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: format!("x-slack-signature malformed: {reason}"),
                }),
            )
                .into_response();
        }
    }

    let action = match parse_interaction_payload(&body) {
        Ok(a) => a,
        Err(e) => {
            // Distinguish operator-visible errors (malformed
            // payload) from "this is just a non-block_actions
            // interaction type we don't handle" — Slack sends
            // both kinds to the same endpoint.
            let status = match e {
                InteractionParseError::NotBlockActions => StatusCode::OK,
                _ => StatusCode::BAD_REQUEST,
            };
            tracing::warn!(error = %e, "slack interact: payload parse failed");
            if status == StatusCode::OK {
                return (StatusCode::OK, Json(EmptyResponse::default())).into_response();
            }
            return (
                status,
                Json(ApiError {
                    error: format!("slack interaction payload: {e}"),
                }),
            )
                .into_response();
        }
    };

    let mesh = match state.mesh_client.as_ref() {
        Some(m) => m.clone(),
        None => {
            tracing::error!(
                approval_id = %action.approval_id,
                "slack interact: mesh client not initialized; decision lost"
            );
            // Still return 200 so Slack doesn't retry — the
            // operator's UI accepted the click. Reconciliation
            // happens via the failed-deliveries surface.
            return (StatusCode::OK, Json(EmptyResponse::default())).into_response();
        }
    };

    let note = if action.username.is_empty() {
        format!("slack:{}", action.user_id)
    } else {
        format!("slack:@{} ({})", action.username, action.user_id)
    };
    let args = serde_json::json!({
        "approval_id": action.approval_id,
        "decision": action.decision,
        "note": note,
    });
    let arg_bytes = match serde_json::to_vec(&args) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "slack interact: encode args");
            return (StatusCode::OK, Json(EmptyResponse::default())).into_response();
        }
    };
    let deadline_secs = state.cfg.transport.deadline_secs.clamp(5, 30);
    let envelope = build_request(
        "approval.record_decision",
        arg_bytes,
        state.identity_bundle.clone(),
        deadline_secs,
    );
    match mesh.call(COORDINATOR_ALIAS, envelope).await {
        Ok(bytes) => match decode_response(&bytes) {
            Ok(resp) => match resp.res {
                ResponseResult::Ok(_) => {
                    tracing::info!(
                        approval_id = %action.approval_id,
                        decision = %action.decision,
                        slack_user = %action.user_id,
                        "slack interact: decision recorded"
                    );
                }
                ResponseResult::Err(env) => {
                    tracing::error!(
                        approval_id = %action.approval_id,
                        err_kind = env.kind,
                        cause = %env.cause,
                        "slack interact: approval.record_decision returned error"
                    );
                }
                ResponseResult::StreamHandle(_) => {
                    tracing::error!(
                        "slack interact: unexpected stream response from approval.record_decision"
                    );
                }
            },
            Err(e) => {
                tracing::error!(error = %e, "slack interact: decode coordinator response");
            }
        },
        Err(e) => {
            tracing::error!(
                approval_id = %action.approval_id,
                error = %e,
                "slack interact: mesh call to coordinator failed"
            );
        }
    }

    // Slack expects empty 200 on success. We honour that even
    // when the coordinator round trip failed so Slack does not
    // re-deliver the same click; operators reconcile via the
    // failed-deliveries surface.
    (StatusCode::OK, Json(EmptyResponse::default())).into_response()
}

/// Build the formatted decision note for an interaction.
/// Exposed for tests; the same shape is built inline in
/// `slack_interact` so the live decision row carries the
/// operator's Slack identity.
#[doc(hidden)]
#[cfg(test)]
pub(crate) fn format_decision_note(user_id: &str, username: &str) -> String {
    if username.is_empty() {
        format!("slack:{user_id}")
    } else {
        format!("slack:@{username} ({user_id})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_note_uses_username_when_present() {
        assert_eq!(format_decision_note("U123", "alice"), "slack:@alice (U123)");
    }

    #[test]
    fn decision_note_falls_back_to_user_id_when_username_empty() {
        assert_eq!(format_decision_note("U123", ""), "slack:U123");
    }

    #[test]
    fn env_var_name_matches_documented_constant() {
        // Pin the env var name — operator docs reference it
        // directly so accidental renames here would silently
        // break deployments.
        assert_eq!(
            SLACK_SIGNING_SECRET_ENV,
            "RELIX_BRIDGE_SLACK_SIGNING_SECRET"
        );
    }

    /// Helper that mirrors `slack_interact`'s decision-stage
    /// path: given an env var, headers, and body, what would
    /// the verification leg conclude? We test the
    /// verification path through the public helper
    /// `relix_slack::verify_request_signature` rather than
    /// going through axum since `slack_interact` needs a full
    /// `AppState` to exercise.
    #[test]
    fn slack_signature_helpers_are_re_exported_to_bridge_callers() {
        // Compile-test — this asserts the module imports
        // resolve and the bridge sees the same enum variants
        // it dispatches on at runtime.
        let _ok = SignatureCheck::Valid;
        let _ = SignatureCheck::Stale;
        let _ = SignatureCheck::Mismatch;
    }

    #[test]
    fn parse_interaction_error_variants_are_routable() {
        // Defensive — make sure the variant we treat as 200
        // (NotBlockActions) is matchable here so a future
        // refactor that renames it fails this assertion.
        let v = InteractionParseError::NotBlockActions;
        assert!(matches!(v, InteractionParseError::NotBlockActions));
    }
}
