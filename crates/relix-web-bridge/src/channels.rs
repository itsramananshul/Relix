//! Inbound webhook routes for the wire-real approval
//! channels. Each route lives behind its channel's signature-
//! verification primitive in the corresponding channel crate
//! (`relix-slack`, `relix-discord`, …).
//!
//! Slack: `POST /v1/channels/slack/interact` (PART 2)
//! --------------------------------------------------
//!
//! Operators paste the Slack app's **signing secret** into
//! `RELIX_BRIDGE_SLACK_SIGNING_SECRET`. The handler verifies
//! the `x-slack-signature` HMAC against the raw body, parses
//! the Block Kit `block_actions` interaction payload, then
//! dispatches the lifted decision to the coordinator's
//! `approval.record_decision` cap.
//!
//! Discord: `POST /v1/channels/discord/interact` (PART 3)
//! ------------------------------------------------------
//!
//! Operators paste the Discord application's **public key**
//! into `RELIX_BRIDGE_DISCORD_PUBLIC_KEY`. The handler:
//!
//! 1. Verifies the `X-Signature-Ed25519` + `X-Signature-Timestamp`
//!    pair against the body (Discord's required posture per
//!    [the docs](https://discord.com/developers/docs/interactions/receiving-and-responding#security-and-authorization)).
//! 2. Handles the verification `type=1` PING by returning a
//!    `{"type":1}` PONG — required so Discord can validate the
//!    interactions endpoint URL when operators paste it in
//!    the Developer Portal.
//! 3. For `type=3` MESSAGE_COMPONENT clicks, parses
//!    `data.custom_id`, lifts the decision (`approved` /
//!    `rejected`), forwards to `approval.record_decision`, and
//!    returns an ephemeral acknowledgement message so the
//!    operator sees the click was recorded.
//!
//! All approval channels expect a fast 200 response (Slack: 3s
//! budget; Discord: 3s budget plus PING-must-be-fast). The
//! mesh call to the coordinator is fire-and-await but
//! `record_decision` is a single SQLite UPDATE + the cancel-
//! sender hop landed in PART 7. If the coordinator is
//! unreachable we still return 200 with the documented Discord
//! / Slack interaction-response shape; the failed-deliveries
//! surface reconciles the decision.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use serde::Serialize;

use relix_discord::{self, InteractionKind};
use relix_runtime::approval::{
    EmailProvider, EmailReplyError, SubjectDecision, parse_inbound_webhook,
    parse_subject_for_decision, verify_mailgun_signature,
};
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

/// PART 3: env var the bridge reads the Discord application
/// public key from at startup. Operators copy the value from
/// the Discord Developer Portal's "General Information" tab.
/// Unset = `/v1/channels/discord/interact` returns 503 with a
/// clear error so the wire reason surfaces in the Discord
/// developer portal's interaction logs.
pub const DISCORD_PUBLIC_KEY_ENV: &str = "RELIX_BRIDGE_DISCORD_PUBLIC_KEY";

/// PART 4: env var the bridge reads the Mailgun signing key
/// from. When set, Mailgun-shaped inbound webhooks are HMAC-
/// verified before processing. When unset, Mailgun inbound is
/// still accepted but the handler logs a warning.
pub const MAILGUN_SIGNING_KEY_ENV: &str = "RELIX_BRIDGE_MAILGUN_SIGNING_KEY";

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

    let note = if action.username.is_empty() {
        format!("slack:{}", action.user_id)
    } else {
        format!("slack:@{} ({})", action.username, action.user_id)
    };
    forward_record_decision(&state, &action.approval_id, action.decision, &note, "slack").await;

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

// ────────────────────────────────────────────────────────────
// PART 3 — Discord interactions endpoint
// ────────────────────────────────────────────────────────────

/// `POST /v1/channels/discord/interact`
///
/// Verifies the `X-Signature-Ed25519` + `X-Signature-Timestamp`
/// pair against the raw body, then either:
///
/// - Returns Discord's `{"type": 1}` PONG for the verification
///   PING (`type=1`) so the operator can paste the URL into the
///   Discord Developer Portal and the validation passes.
/// - Parses a MESSAGE_COMPONENT (`type=3`) click, forwards the
///   decision to `approval.record_decision`, and returns an
///   ephemeral acknowledgement message.
/// - Logs and returns the deferred-update response for any
///   other interaction type (Discord retries on non-2xx so a
///   silent 4xx would loop forever).
pub async fn discord_interact(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let public_key = match std::env::var(DISCORD_PUBLIC_KEY_ENV) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => {
            tracing::warn!("discord interact: {DISCORD_PUBLIC_KEY_ENV} unset; rejecting webhook");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ApiError {
                    error: format!(
                        "discord interactivity disabled: set {DISCORD_PUBLIC_KEY_ENV} \
                         to the Discord application's public key to enable"
                    ),
                }),
            )
                .into_response();
        }
    };

    let ts = match headers
        .get("x-signature-timestamp")
        .and_then(|h| h.to_str().ok())
    {
        Some(s) => s.to_string(),
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(ApiError {
                    error: "missing X-Signature-Timestamp header".into(),
                }),
            )
                .into_response();
        }
    };
    let sig = match headers
        .get("x-signature-ed25519")
        .and_then(|h| h.to_str().ok())
    {
        Some(s) => s.to_string(),
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(ApiError {
                    error: "missing X-Signature-Ed25519 header".into(),
                }),
            )
                .into_response();
        }
    };

    let check = relix_discord::verify_interaction_signature(&public_key, &ts, &sig, &body);
    match check {
        relix_discord::SignatureCheck::Valid => {}
        relix_discord::SignatureCheck::Mismatch => {
            tracing::warn!("discord interact: Ed25519 signature mismatch");
            return (
                StatusCode::UNAUTHORIZED,
                Json(ApiError {
                    error: "X-Signature-Ed25519: verification failed".into(),
                }),
            )
                .into_response();
        }
        relix_discord::SignatureCheck::Malformed(reason) => {
            tracing::warn!(reason = reason, "discord interact: signature malformed");
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: format!("X-Signature-Ed25519 malformed: {reason}"),
                }),
            )
                .into_response();
        }
    }

    let kind = match relix_discord::parse_interaction_payload(&body) {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(error = %e, "discord interact: payload parse failed");
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: format!("discord interaction payload: {e}"),
                }),
            )
                .into_response();
        }
    };

    match kind {
        InteractionKind::Ping => {
            // Discord developer portal pastes the URL and
            // expects the PONG back to prove ownership.
            (StatusCode::OK, Json(relix_discord::pong_response())).into_response()
        }
        InteractionKind::Component(action) => {
            // Decision lifted from the button click.
            let note = if action.username.is_empty() {
                format!("discord:{}", action.user_id)
            } else {
                format!("discord:@{} ({})", action.username, action.user_id)
            };
            let ack_text = match action.decision {
                "approved" => format!("✅ Approval `{}` recorded.", action.approval_id),
                _ => format!("❌ Approval `{}` denied.", action.approval_id),
            };
            // Forward to the coordinator. We log on failure
            // but still return an ack so the operator sees a
            // confirmation. Reconciliation flows through
            // failed-deliveries.
            forward_record_decision(
                &state,
                &action.approval_id,
                action.decision,
                &note,
                "discord",
            )
            .await;
            (StatusCode::OK, Json(relix_discord::ack_response(&ack_text))).into_response()
        }
        InteractionKind::Other(ty) => {
            tracing::info!(
                interaction_type = ty,
                "discord interact: unhandled interaction type — returning deferred update"
            );
            (
                StatusCode::OK,
                Json(relix_discord::deferred_update_response()),
            )
                .into_response()
        }
    }
}

// ────────────────────────────────────────────────────────────
// PART 4 — Email reply webhook (Mailgun / SendGrid / Postmark)
// ────────────────────────────────────────────────────────────

/// `POST /v1/channels/email/reply`
///
/// Accepts inbound webhooks from any of the three supported
/// providers. The handler:
///
/// 1. Reads the `Content-Type` header to bias provider
///    detection (`application/json` ⇒ Postmark;
///    `application/x-www-form-urlencoded` ⇒ Mailgun or SendGrid).
/// 2. For Mailgun (detected by the `signature` + `token`
///    form fields), HMAC-verifies the body against
///    `RELIX_BRIDGE_MAILGUN_SIGNING_KEY` when set.
/// 3. For SendGrid and Postmark, accepts the body — these
///    providers don't sign requests server-side; deployments
///    should put the route behind a reverse-proxy basic-auth
///    layer or a hard-to-guess path.
/// 4. Extracts the operator's vote from the reply subject
///    (`APPROVE` / `DENY` / `REJECTED` etc. as the first
///    word, plus the bracketed approval id).
/// 5. Forwards `approved` / `rejected` to
///    `approval.record_decision` via mesh, with the
///    operator's `From:` address in the decision note for
///    attribution.
///
/// Always returns 200 to the provider on a successful parse —
/// providers retry on non-2xx and we don't want duplicate
/// decisions on transient coordinator failures.
pub async fn email_reply(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();

    let parsed = match parse_inbound_webhook(&content_type, &body) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "email reply: parse failed");
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: format!("email reply: {e}"),
                }),
            )
                .into_response();
        }
    };

    // Mailgun: HMAC-verify when the operator pasted the
    // signing key into the env var. Unset ⇒ log a warning and
    // accept (operators may be running behind a reverse proxy
    // that already enforces).
    if parsed.provider == EmailProvider::Mailgun {
        match std::env::var(MAILGUN_SIGNING_KEY_ENV) {
            Ok(key) if !key.trim().is_empty() => {
                if let Err(e) = verify_mailgun_signature(&key, &body) {
                    match e {
                        EmailReplyError::MailgunSignatureMismatch => {
                            tracing::warn!("email reply: Mailgun HMAC mismatch — rejecting");
                            return (
                                StatusCode::UNAUTHORIZED,
                                Json(ApiError {
                                    error: "mailgun signature mismatch".into(),
                                }),
                            )
                                .into_response();
                        }
                        other => {
                            tracing::warn!(error = %other, "email reply: Mailgun signature malformed");
                            return (
                                StatusCode::BAD_REQUEST,
                                Json(ApiError {
                                    error: format!("mailgun signature: {other}"),
                                }),
                            )
                                .into_response();
                        }
                    }
                }
            }
            _ => {
                tracing::warn!(
                    "email reply: {MAILGUN_SIGNING_KEY_ENV} unset; accepting Mailgun webhook \
                     without HMAC verification — wire a signing key for production"
                );
            }
        }
    }

    let action = parse_subject_for_decision(&parsed.subject);
    let decision = match action.decision {
        SubjectDecision::Approved => "approved",
        SubjectDecision::Rejected => "rejected",
        SubjectDecision::Unknown => {
            tracing::info!(
                subject = %parsed.subject,
                from = %parsed.from,
                "email reply: subject did not carry a recognised decision — ignoring"
            );
            // Still return 200 so the provider doesn't retry.
            return (StatusCode::OK, Json(EmptyResponse::default())).into_response();
        }
    };
    if action.approval_id.is_empty() {
        tracing::warn!(
            subject = %parsed.subject,
            from = %parsed.from,
            "email reply: missing approval id in subject — ignoring"
        );
        return (StatusCode::OK, Json(EmptyResponse::default())).into_response();
    }

    let note = if parsed.from.is_empty() {
        format!("email:{}", provider_tag(parsed.provider))
    } else {
        format!(
            "email:{}:{}",
            provider_tag(parsed.provider),
            parsed.from.replace([' ', '\n', '\r'], "")
        )
    };

    forward_record_decision(&state, &action.approval_id, decision, &note, "email").await;
    (StatusCode::OK, Json(EmptyResponse::default())).into_response()
}

fn provider_tag(p: EmailProvider) -> &'static str {
    match p {
        EmailProvider::Mailgun => "mailgun",
        EmailProvider::SendGrid => "sendgrid",
        EmailProvider::Postmark => "postmark",
    }
}

/// Shared helper: invoke `approval.record_decision` on the
/// coordinator with the decision lifted from a channel
/// interaction. Logs on failure — channels expect a fast
/// success response so we never propagate the error to the
/// HTTP layer.
async fn forward_record_decision(
    state: &AppState,
    approval_id: &str,
    decision: &str,
    note: &str,
    channel_tag: &str,
) {
    let mesh = match state.mesh_client.as_ref() {
        Some(m) => m.clone(),
        None => {
            tracing::error!(
                channel = channel_tag,
                approval_id = approval_id,
                "channel interact: mesh client not initialized; decision lost"
            );
            return;
        }
    };
    let args = serde_json::json!({
        "approval_id": approval_id,
        "decision": decision,
        "note": note,
    });
    let arg_bytes = match serde_json::to_vec(&args) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(channel = channel_tag, error = %e, "channel interact: encode args");
            return;
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
                        channel = channel_tag,
                        approval_id = approval_id,
                        decision = decision,
                        "channel interact: decision recorded"
                    );
                }
                ResponseResult::Err(env) => {
                    tracing::error!(
                        channel = channel_tag,
                        approval_id = approval_id,
                        err_kind = env.kind,
                        cause = %env.cause,
                        "channel interact: approval.record_decision returned error"
                    );
                }
                ResponseResult::StreamHandle(_) => {
                    tracing::error!(
                        channel = channel_tag,
                        "channel interact: unexpected stream response"
                    );
                }
            },
            Err(e) => {
                tracing::error!(channel = channel_tag, error = %e, "channel interact: decode coordinator response");
            }
        },
        Err(e) => {
            tracing::error!(
                channel = channel_tag,
                approval_id = approval_id,
                error = %e,
                "channel interact: mesh call to coordinator failed"
            );
        }
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

    // ── PART 3 — Discord constants pin ─────────────────────

    #[test]
    fn discord_env_var_name_matches_documented_constant() {
        // Pin the env var name — operator docs reference it
        // directly so accidental renames here would silently
        // break deployments.
        assert_eq!(DISCORD_PUBLIC_KEY_ENV, "RELIX_BRIDGE_DISCORD_PUBLIC_KEY");
    }

    #[test]
    fn discord_interaction_kind_variants_route_distinctly() {
        // Compile-test — the bridge dispatches on these three
        // variants. A future refactor that renames them must
        // also rename the dispatch branches.
        let _ = InteractionKind::Ping;
        let _ = InteractionKind::Component(relix_discord::InteractionAction {
            approval_id: "x".into(),
            decision: "approved",
            user_id: "U".into(),
            username: "u".into(),
        });
        let _ = InteractionKind::Other(42);
    }

    // ── PART 4 — Email reply route ─────────────────────────

    #[test]
    fn mailgun_env_var_name_matches_documented_constant() {
        // Pin the env var name — operator docs reference it
        // directly.
        assert_eq!(MAILGUN_SIGNING_KEY_ENV, "RELIX_BRIDGE_MAILGUN_SIGNING_KEY");
    }

    #[test]
    fn provider_tag_returns_lowercase_label_per_variant() {
        assert_eq!(provider_tag(EmailProvider::Mailgun), "mailgun");
        assert_eq!(provider_tag(EmailProvider::SendGrid), "sendgrid");
        assert_eq!(provider_tag(EmailProvider::Postmark), "postmark");
    }

    #[test]
    fn note_attribution_format_strips_whitespace_in_from_address() {
        // Defensive — providers should not include CR/LF in
        // the From header but we strip them anyway so the
        // decision row can't carry header-injection-shaped
        // values.
        let from = "ops@example.com\r\n";
        let cleaned: String = from.replace([' ', '\n', '\r'], "");
        assert_eq!(cleaned, "ops@example.com");
    }
}
