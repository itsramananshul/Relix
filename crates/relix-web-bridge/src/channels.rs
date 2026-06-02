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

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Json,
    body::Bytes,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
};
use serde::Serialize;

use relix_discord::{self, InteractionKind};
use relix_runtime::approval::{
    EmailProvider, EmailReplyError, SubjectDecision, parse_inbound_webhook,
    parse_subject_for_decision, verify_mailgun_signature,
};
use relix_runtime::dispatch::{build_request_with_tenant, decode_response};
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

    if let Err(resp) = verify_slack_signature_or_response(&headers, &body, "interact") {
        return *resp;
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

/// Slack FIX 1 / FIX 2 shared signature gate. Verifies the
/// `x-slack-signature` HMAC against the raw body using the
/// signing secret from `RELIX_BRIDGE_SLACK_SIGNING_SECRET`.
/// Returns `Ok(())` when the signature is valid; otherwise
/// returns the canned axum response the route should reply with
/// (503 when the env var is unset, 401 on stale/mismatch, 400 on
/// malformed). Centralised so `slack_interact` and `slack_events`
/// MUST both apply the same gate — no inbound Slack route can
/// process a payload without going through this verifier.
///
/// Reads the signing secret from the env var; on miss returns a
/// 503 response.
fn verify_slack_signature_or_response(
    headers: &HeaderMap,
    body: &Bytes,
    route_tag: &'static str,
) -> Result<(), Box<axum::response::Response>> {
    use axum::response::IntoResponse;

    let signing_secret = match std::env::var(SLACK_SIGNING_SECRET_ENV) {
        Ok(s) if !s.trim().is_empty() => s,
        _ => {
            tracing::warn!(
                route = route_tag,
                "slack: {SLACK_SIGNING_SECRET_ENV} unset; rejecting webhook"
            );
            return Err(Box::new(
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(ApiError {
                        error: format!(
                            "slack {route_tag}: disabled because {SLACK_SIGNING_SECRET_ENV} \
                             is not set in the bridge environment"
                        ),
                    }),
                )
                    .into_response(),
            ));
        }
    };

    verify_slack_signature_with_secret(&signing_secret, headers, body, route_tag)
}

/// Pure-function inner that the env-reading wrapper delegates
/// to. Tests construct the secret directly + drive every
/// signature-rejection branch without mutating process env.
fn verify_slack_signature_with_secret(
    signing_secret: &str,
    headers: &HeaderMap,
    body: &Bytes,
    route_tag: &'static str,
) -> Result<(), Box<axum::response::Response>> {
    use axum::response::IntoResponse;

    let ts = match headers
        .get("x-slack-request-timestamp")
        .and_then(|h| h.to_str().ok())
    {
        Some(s) => s.to_string(),
        None => {
            return Err(Box::new(
                (
                    StatusCode::UNAUTHORIZED,
                    Json(ApiError {
                        error: "missing x-slack-request-timestamp header".into(),
                    }),
                )
                    .into_response(),
            ));
        }
    };
    let sig = match headers
        .get("x-slack-signature")
        .and_then(|h| h.to_str().ok())
    {
        Some(s) => s.to_string(),
        None => {
            return Err(Box::new(
                (
                    StatusCode::UNAUTHORIZED,
                    Json(ApiError {
                        error: "missing x-slack-signature header".into(),
                    }),
                )
                    .into_response(),
            ));
        }
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    match verify_request_signature(signing_secret, &ts, &sig, body, now) {
        SignatureCheck::Valid => Ok(()),
        SignatureCheck::Stale => {
            tracing::warn!(
                route = route_tag,
                "slack: signature stale (timestamp outside the 5-minute window)"
            );
            Err(Box::new(
                (
                    StatusCode::UNAUTHORIZED,
                    Json(ApiError {
                        error: "x-slack-signature: timestamp outside the 5-minute window".into(),
                    }),
                )
                    .into_response(),
            ))
        }
        SignatureCheck::Mismatch => {
            tracing::warn!(route = route_tag, "slack: HMAC signature mismatch");
            Err(Box::new(
                (
                    StatusCode::UNAUTHORIZED,
                    Json(ApiError {
                        error: "x-slack-signature: HMAC mismatch".into(),
                    }),
                )
                    .into_response(),
            ))
        }
        SignatureCheck::Malformed(reason) => {
            tracing::warn!(
                reason = reason,
                route = route_tag,
                "slack: signature malformed"
            );
            Err(Box::new(
                (
                    StatusCode::BAD_REQUEST,
                    Json(ApiError {
                        error: format!("x-slack-signature malformed: {reason}"),
                    }),
                )
                    .into_response(),
            ))
        }
    }
}

/// `POST /v1/channels/slack/events` — Slack Events API receiver.
///
/// Slack FIX 2. Two payload types matter:
///
/// - `url_verification`: Slack sends this once when the operator
///   pastes the bridge URL into the app's Event Subscriptions
///   settings. The bridge responds with `{ "challenge": "..." }`
///   echoing the random nonce so Slack accepts the URL.
/// - `event_callback`: every real event arrives here. We
///   spawn the processing task off the request thread and
///   respond HTTP 200 immediately — Slack retries (with
///   exponential backoff up to 3 times) on any non-2xx, so a
///   slow downstream peer must NEVER block the response.
///
/// Both paths go through the same signature verification as
/// `slack_interact` so no inbound payload can land without an
/// HMAC check.
pub async fn slack_events(
    State(_state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    if let Err(resp) = verify_slack_signature_or_response(&headers, &body, "events") {
        return *resp;
    }

    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ApiError {
                    error: format!("slack events: payload is not JSON: {e}"),
                }),
            )
                .into_response();
        }
    };

    let kind = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match kind {
        "url_verification" => {
            let challenge = payload
                .get("challenge")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if challenge.is_empty() {
                tracing::warn!(
                    "slack events: url_verification arrived without a `challenge` field"
                );
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ApiError {
                        error: "slack url_verification: missing `challenge`".into(),
                    }),
                )
                    .into_response();
            }
            tracing::info!(
                challenge_len = challenge.len(),
                "slack events: url_verification handshake completed"
            );
            (StatusCode::OK, Json(SlackChallengeResponse { challenge })).into_response()
        }
        "event_callback" => {
            // Spawn the downstream processing off-thread so the
            // 200 lands within Slack's 3s retry budget no matter
            // how slow the coordinator hop is. We log the event
            // subtype so operators see what they're receiving;
            // wiring the event to the controller is deliberately
            // out-of-scope for FIX 2 (the spec is the route +
            // signature verification + url_verification +
            // event_callback fast-200).
            let event_kind = payload
                .get("event")
                .and_then(|e| e.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let team_id = payload
                .get("team_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            tokio::spawn(async move {
                tracing::info!(
                    event_kind = %event_kind,
                    team_id = %team_id,
                    "slack events: event_callback dispatched"
                );
            });
            (StatusCode::OK, Json(EmptyResponse::default())).into_response()
        }
        other => {
            tracing::warn!(
                kind = other,
                "slack events: unhandled payload type; acking 200 to suppress retry"
            );
            (StatusCode::OK, Json(EmptyResponse::default())).into_response()
        }
    }
}

#[derive(Debug, Serialize)]
struct SlackChallengeResponse {
    challenge: String,
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
    let tenant = crate::tenant::current_tenant_or_none();
    let envelope = build_request_with_tenant(
        "approval.record_decision",
        arg_bytes,
        state.identity_bundle.clone(),
        deadline_secs,
        None,
        None,
        None,
        tenant.clone(),
    );
    match mesh.call(COORDINATOR_ALIAS, envelope).await {
        Ok(bytes) => match decode_response(&bytes) {
            Ok(resp) => match resp.res {
                ResponseResult::Ok(_) => {
                    let tenant_id = tenant.clone().unwrap_or_else(|| "default".into());
                    let detail = if note.trim().is_empty() {
                        format!("approval decision recorded via {channel_tag}")
                    } else {
                        note.to_string()
                    };
                    if let Err(e) = crate::activity::append_approval_activity(
                        state.cfg.transport.data_dir.as_deref(),
                        &tenant_id,
                        channel_tag,
                        approval_id,
                        decision,
                        None,
                        detail,
                    ) {
                        tracing::warn!(
                            channel = channel_tag,
                            approval_id = approval_id,
                            decision = decision,
                            error = %e,
                            "channel interact: decision recorded but activity ledger append failed"
                        );
                    }
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

// ────────────────────────────────────────────────────────────
// Telegram FIX 1 — webhook receiver
// ────────────────────────────────────────────────────────────

/// Telegram's published source-IP ranges. Updates POST'd from
/// any address NOT in these ranges are rejected with HTTP 401.
/// Documented at <https://core.telegram.org/bots/webhooks>.
/// The two CIDR blocks are 149.154.160.0/20 (legacy datacenter)
/// and 91.108.4.0/22 (newer datacenter).
const TELEGRAM_CIDR_BLOCKS: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::new(149, 154, 160, 0), 20),
    (Ipv4Addr::new(91, 108, 4, 0), 22),
];

/// Returns `true` when `ip` falls inside one of Telegram's
/// published IPv4 source ranges. IPv6 addresses are rejected
/// (Telegram does not currently send webhooks over IPv6).
pub(crate) fn is_telegram_source_ip(ip: IpAddr) -> bool {
    let IpAddr::V4(v4) = ip else {
        return false;
    };
    let octets = v4.octets();
    let ip_u32 = u32::from_be_bytes(octets);
    for &(base, prefix) in TELEGRAM_CIDR_BLOCKS {
        let base_u32 = u32::from_be_bytes(base.octets());
        let mask: u32 = if prefix == 0 {
            0
        } else {
            (!0u32) << (32 - prefix)
        };
        if (ip_u32 & mask) == (base_u32 & mask) {
            return true;
        }
    }
    false
}

/// `POST /v1/channels/telegram/webhook`
///
/// Telegram FIX 1. Receives Update payloads from Telegram when
/// the bot is in webhook mode (i.e. the operator pasted the
/// bridge's public URL into `setWebhook`). Contract:
///
/// 1. Verify the source IP falls in Telegram's published
///    ranges via [`is_telegram_source_ip`]. Reject HTTP 401
///    otherwise.
/// 2. Parse the body as JSON (Telegram's Update shape). Reject
///    HTTP 400 on malformed JSON.
/// 3. Forward the raw body to the Telegram peer via mesh
///    `telegram.webhook_update` so the same code path that
///    handles `get_updates` runs against the inbound message.
/// 4. Respond HTTP 200 IMMEDIATELY — Telegram retries after 5s
///    on any non-2xx, so a slow downstream peer must never
///    block the response.
///
/// The forwarding hop is `tokio::spawn`-ed; the response goes
/// out before the mesh call lands. A failure on the mesh leg
/// is logged at WARN (the bridge cannot re-deliver after the
/// 200 is on the wire — operators reconcile via the failed-
/// deliveries surface).
pub async fn telegram_webhook(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let ip = addr.ip();
    if !is_telegram_source_ip(ip) {
        tracing::warn!(
            source_ip = %ip,
            "telegram webhook: source IP not in Telegram's published ranges; rejecting"
        );
        return (
            StatusCode::UNAUTHORIZED,
            Json(ApiError {
                error: format!(
                    "telegram webhook: source IP {ip} is not in Telegram's \
                     published ranges (149.154.160.0/20 + 91.108.4.0/22)"
                ),
            }),
        )
            .into_response();
    }

    // Validate the body parses as JSON before acknowledging.
    // A malformed body indicates either misconfiguration or a
    // spoofed request that slipped through the IP check, so
    // surfacing a 400 is more useful than 200.
    if let Err(e) = serde_json::from_slice::<serde_json::Value>(&body) {
        tracing::warn!(error = %e, "telegram webhook: body is not JSON");
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: format!("telegram webhook: body is not JSON: {e}"),
            }),
        )
            .into_response();
    }

    // Forward to the Telegram peer via mesh. Spawned so the
    // 200 lands within Telegram's 5s budget regardless of
    // peer latency.
    let state_for_spawn = state.clone();
    let body_for_spawn = body.clone();
    // PART 3: capture the request-task's resolved tenant id
    // BEFORE the `tokio::spawn` so it crosses the task
    // boundary as a regular value. The task-local
    // `CURRENT_TENANT` does NOT propagate into spawned
    // futures — without this capture the forwarded envelope
    // would carry `tenant_id = None` and the downstream
    // would silently route to the default tenant.
    let tenant_for_spawn = crate::tenant::current_tenant_or_none();
    tokio::spawn(async move {
        let mesh = match state_for_spawn.mesh_client.as_ref() {
            Some(m) => m.clone(),
            None => {
                tracing::warn!("telegram webhook: mesh client not initialised; dropped Update");
                return;
            }
        };
        let envelope = relix_runtime::dispatch::build_request_with_tenant(
            "telegram.webhook_update",
            body_for_spawn.to_vec(),
            state_for_spawn.identity_bundle.clone(),
            state_for_spawn.cfg.transport.deadline_secs.clamp(5, 30),
            None,
            None,
            None,
            tenant_for_spawn,
        );
        if let Err(e) = mesh.call("telegram", envelope).await {
            tracing::warn!(
                error = %e,
                "telegram webhook: forward to telegram peer via mesh failed"
            );
        }
    });

    (StatusCode::OK, Json(EmptyResponse::default())).into_response()
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

    // ── Slack FIX 2 — events endpoint ──────────────────────

    /// FIX 2: missing `x-slack-request-timestamp` is rejected
    /// with HTTP 401.
    #[test]
    fn fix2_signature_helper_rejects_missing_timestamp_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-slack-signature", "v0=deadbeef".parse().unwrap());
        let body = Bytes::from_static(b"{}");
        let result = verify_slack_signature_with_secret("test-secret", &headers, &body, "events");
        let resp = result.expect_err("must reject when ts header missing");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// FIX 2: missing `x-slack-signature` is rejected with
    /// HTTP 401.
    #[test]
    fn fix2_signature_helper_rejects_missing_signature_header() {
        let mut headers = HeaderMap::new();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        headers.insert(
            "x-slack-request-timestamp",
            now.to_string().parse().unwrap(),
        );
        let body = Bytes::from_static(b"{}");
        let result = verify_slack_signature_with_secret("test-secret", &headers, &body, "events");
        let resp = result.expect_err("must reject when signature header missing");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// FIX 2: stale timestamp (> 5min old) gets HTTP 401.
    #[test]
    fn fix2_signature_helper_rejects_stale_timestamp() {
        let mut headers = HeaderMap::new();
        let old_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 6 * 60; // 6 minutes ago
        headers.insert(
            "x-slack-request-timestamp",
            old_ts.to_string().parse().unwrap(),
        );
        // A valid-looking signature shape so the verifier
        // proceeds past parse and lands on the stale branch.
        headers.insert("x-slack-signature", "v0=00".parse().unwrap());
        let body = Bytes::from_static(b"{}");
        let result = verify_slack_signature_with_secret("test-secret", &headers, &body, "events");
        let resp = result.expect_err("must reject stale timestamp");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// FIX 2: bad-shape signature (no `v0=` prefix) yields
    /// HTTP 400 — distinct from "missing" (401) so operators
    /// can tell apart a misconfigured app from a missing
    /// header.
    #[test]
    fn fix2_signature_helper_rejects_malformed_signature() {
        let mut headers = HeaderMap::new();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        headers.insert(
            "x-slack-request-timestamp",
            now.to_string().parse().unwrap(),
        );
        // Garbage signature — no `v0=` prefix.
        headers.insert("x-slack-signature", "garbage".parse().unwrap());
        let body = Bytes::from_static(b"{}");
        let result = verify_slack_signature_with_secret("test-secret", &headers, &body, "events");
        let resp = result.expect_err("must reject malformed signature");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// FIX 2: valid signature passes through with `Ok(())`.
    /// We compute the HMAC the same way `verify_request_signature`
    /// does so the round trip succeeds.
    #[test]
    fn fix2_signature_helper_accepts_valid_signature() {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let secret = "test-secret";
        let body_bytes = b"{\"type\":\"event_callback\"}";
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let basestring = format!("v0:{now}:{}", String::from_utf8_lossy(body_bytes));
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(basestring.as_bytes());
        let sig_hex = hex::encode(mac.finalize().into_bytes());
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-slack-request-timestamp",
            now.to_string().parse().unwrap(),
        );
        headers.insert(
            "x-slack-signature",
            format!("v0={sig_hex}").parse().unwrap(),
        );
        let body = Bytes::copy_from_slice(body_bytes);
        let result = verify_slack_signature_with_secret(secret, &headers, &body, "events");
        assert!(
            result.is_ok(),
            "valid signature must pass; got {:?}",
            result.err().map(|r| r.status())
        );
    }

    // ── Telegram FIX 1 — webhook IP allowlist ──────────────

    /// FIX 1: every Telegram-published IP in 149.154.160.0/20
    /// passes. Sample addresses chosen at each boundary of the
    /// /20 block: low end, mid range, high end. The /20 prefix
    /// covers 149.154.160.0 through 149.154.175.255 (4096
    /// addresses total).
    #[test]
    fn fix1_telegram_ip_block_149_154_admits_addresses_in_range() {
        for addr in &[
            "149.154.160.0",
            "149.154.160.1",
            "149.154.167.42",
            "149.154.175.254",
            "149.154.175.255",
        ] {
            let ip: IpAddr = addr.parse().unwrap();
            assert!(
                is_telegram_source_ip(ip),
                "address {addr} must be admitted (149.154.160.0/20)"
            );
        }
    }

    #[test]
    fn fix1_telegram_ip_block_91_108_admits_addresses_in_range() {
        // /22 covers 91.108.4.0 through 91.108.7.255 (1024
        // addresses total).
        for addr in &[
            "91.108.4.0",
            "91.108.4.1",
            "91.108.6.42",
            "91.108.7.254",
            "91.108.7.255",
        ] {
            let ip: IpAddr = addr.parse().unwrap();
            assert!(
                is_telegram_source_ip(ip),
                "address {addr} must be admitted (91.108.4.0/22)"
            );
        }
    }

    #[test]
    fn fix1_telegram_ip_block_rejects_addresses_just_outside_range() {
        // One address below + above each block. These are the
        // critical "off-by-one" cases that a naive comparison
        // (e.g. checking only the first three octets) would
        // miss.
        for addr in &[
            "149.154.159.255", // just below 149.154.160.0/20
            "149.154.176.0",   // just above 149.154.175.255
            "91.108.3.255",    // just below 91.108.4.0/22
            "91.108.8.0",      // just above 91.108.7.255
            "8.8.8.8",         // far outside
            "192.168.1.1",     // RFC 1918
            "127.0.0.1",       // loopback
            "169.254.1.1",     // link-local
        ] {
            let ip: IpAddr = addr.parse().unwrap();
            assert!(
                !is_telegram_source_ip(ip),
                "address {addr} must be rejected"
            );
        }
    }

    #[test]
    fn fix1_telegram_ip_rejects_ipv6_addresses() {
        // Telegram does not currently send webhooks over IPv6
        // — when they start, the allowlist needs both this
        // function AND the documented ranges updated.
        let v6: IpAddr = "::1".parse().unwrap();
        assert!(!is_telegram_source_ip(v6));
        let v6_global: IpAddr = "2001:db8::1".parse().unwrap();
        assert!(!is_telegram_source_ip(v6_global));
    }

    #[test]
    fn fix1_telegram_ip_documented_constants_match_spec() {
        // Pin the two CIDR blocks documented in the FIX 1
        // spec. A future Telegram range change must update
        // both this test AND the operator docs.
        assert_eq!(TELEGRAM_CIDR_BLOCKS.len(), 2);
        assert_eq!(
            TELEGRAM_CIDR_BLOCKS[0],
            (Ipv4Addr::new(149, 154, 160, 0), 20)
        );
        assert_eq!(TELEGRAM_CIDR_BLOCKS[1], (Ipv4Addr::new(91, 108, 4, 0), 22));
    }

    #[test]
    fn fix2_slack_challenge_response_serialises_with_single_field() {
        // FIX 2: the documented contract — the `slack_events`
        // handler MUST respond to `url_verification` with a
        // JSON body shaped exactly `{"challenge":"..."}`. Lock
        // the wire shape here so a future refactor that adds
        // extra fields would break this test.
        let r = SlackChallengeResponse {
            challenge: "nonce-1234".into(),
        };
        let j = serde_json::to_string(&r).expect("serialise");
        assert_eq!(j, r#"{"challenge":"nonce-1234"}"#);
    }
}
