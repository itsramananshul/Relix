//! `SlackChannelDispatch` — wire-real implementation of
//! [`SingleChannelDispatch`] for Slack.
//!
//! Renders an approval request as a Block Kit message with
//! Approve / Deny buttons in an `actions` block. The buttons'
//! `value` field embeds the approval id using the
//! `approve:<id>` / `deny:<id>` syntax; Slack echoes the
//! value back on the interaction payload the operator's
//! click produces, the bridge verifies the `x-slack-signature`
//! HMAC, and routes the decision to the coordinator's
//! `approval.record_decision` cap.
//!
//! This module also exposes the wire-level helpers the bridge
//! consumes for the inbound interaction route:
//!
//! - [`verify_request_signature`] — constant-time HMAC-SHA256
//!   verification of `x-slack-signature` against the operator-
//!   configured signing secret. Built-in replay protection
//!   rejects timestamps older than 5 minutes (Slack's
//!   documented recommendation).
//! - [`parse_interaction_payload`] — decodes the
//!   `application/x-www-form-urlencoded` body Slack sends to
//!   the interactivity endpoint, extracts the
//!   [`InteractionAction`] (approve / deny + approval id), and
//!   returns enough metadata for the bridge to attribute the
//!   decision (operator's user id + username).

use std::sync::Arc;

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use relix_core::approval::{ApprovalRequest, ChannelDispatchError, SingleChannelDispatch};

use crate::{OutgoingMessage, SlackApi};

/// Max age of a Slack interaction payload — anything older is
/// rejected as a replay. Five minutes mirrors Slack's
/// recommendation in the verifying-requests guide.
pub const MAX_SIGNATURE_AGE_SECS: i64 = 60 * 5;

/// Block Kit action prefix carried in button `value` fields.
/// The bridge's interaction handler matches against these
/// when parsing the payload.
pub const APPROVE_VALUE_PREFIX: &str = "approve:";
/// Block Kit action prefix for the Deny button.
pub const DENY_VALUE_PREFIX: &str = "deny:";

/// Wire-real per-channel dispatcher. Holds the
/// [`SlackApi`] handle behind an [`Arc`] so the controller's
/// startup wires one [`crate::LiveSlackApi`] and hands it to
/// the receive loop AND the approval pipeline. `channel_id`
/// is the Slack channel the operator chose for approval
/// notifications (`C…` / `G…` / `D…`).
#[derive(Clone)]
pub struct SlackChannelDispatch {
    api: Arc<dyn SlackApi>,
    channel_id: String,
}

impl SlackChannelDispatch {
    /// Construct a new dispatcher. Caller has already
    /// validated that `channel_id` is non-empty and the bot
    /// has been invited.
    pub fn new(api: Arc<dyn SlackApi>, channel_id: String) -> Self {
        Self { api, channel_id }
    }

    /// Build the Block Kit `blocks` array for one approval
    /// request. Exposed for testing — production callers go
    /// through [`SingleChannelDispatch::send`].
    pub fn build_blocks(request: &ApprovalRequest, is_escalation: bool) -> Vec<serde_json::Value> {
        let heading = if is_escalation {
            ":rotating_light: *ESCALATED Approval Required*"
        } else {
            ":lock: *Approval Required*"
        };
        let body = format!(
            "{heading}\n\n*Agent:* {agent}\n*Action:* `{capability}`\n*Request:* {summary}\n\
             *Session:* `{session}`\n*Approval ID:* `{id}`",
            agent = request.agent_name,
            capability = request.capability,
            summary = request.request_summary,
            session = request.session_id,
            id = request.approval_id,
        );
        vec![
            serde_json::json!({
                "type": "section",
                "text": { "type": "mrkdwn", "text": body },
            }),
            serde_json::json!({
                "type": "actions",
                "block_id": format!("approval:{}", request.approval_id),
                "elements": [
                    {
                        "type": "button",
                        "action_id": "approval_approve",
                        "style": "primary",
                        "text": { "type": "plain_text", "text": "Approve", "emoji": true },
                        "value": format!("{APPROVE_VALUE_PREFIX}{}", request.approval_id),
                    },
                    {
                        "type": "button",
                        "action_id": "approval_deny",
                        "style": "danger",
                        "text": { "type": "plain_text", "text": "Deny", "emoji": true },
                        "value": format!("{DENY_VALUE_PREFIX}{}", request.approval_id),
                    },
                ],
            }),
        ]
    }

    /// Render the plain-text fallback used in notifications
    /// and clients that can't render blocks. Exposed for
    /// testing.
    pub fn render_fallback_text(request: &ApprovalRequest, is_escalation: bool) -> String {
        let heading = if is_escalation {
            "ESCALATED Approval Required"
        } else {
            "Approval Required"
        };
        format!(
            "{heading} — agent={agent} action={capability} approval_id={id}",
            agent = request.agent_name,
            capability = request.capability,
            id = request.approval_id,
        )
    }
}

#[async_trait]
impl SingleChannelDispatch for SlackChannelDispatch {
    async fn send(
        &self,
        request: &ApprovalRequest,
        is_escalation: bool,
    ) -> Result<(), ChannelDispatchError> {
        if self.channel_id.is_empty() {
            return Err(ChannelDispatchError::Disabled("slack".into()));
        }
        let out = OutgoingMessage {
            channel_id: self.channel_id.clone(),
            thread_ts: String::new(),
            text: Self::render_fallback_text(request, is_escalation),
            blocks: Self::build_blocks(request, is_escalation),
        };
        self.api
            .chat_post_message(&out)
            .await
            .map_err(|e| ChannelDispatchError::Transport(format!("slack: {e}")))
    }
}

// ── inbound interaction verification ─────────────────────

/// Result of verifying a Slack-signed request.
#[derive(Debug, PartialEq, Eq)]
pub enum SignatureCheck {
    /// Signature parsed AND the HMAC matched AND the
    /// timestamp is within [`MAX_SIGNATURE_AGE_SECS`] of `now`.
    Valid,
    /// Header is missing or has the wrong shape.
    Malformed(&'static str),
    /// Header parsed but the timestamp is too old / too new —
    /// rejected as a replay.
    Stale,
    /// Header parsed and timestamp is fresh, but the HMAC did
    /// not match the signing secret.
    Mismatch,
}

/// Verify the `x-slack-signature` HMAC against a raw request
/// body and timestamp. `now_unix_secs` is taken as a parameter
/// so tests can run deterministically. The comparison is
/// constant-time (HMAC's `verify_slice` uses
/// [`subtle::ConstantTimeEq`] under the hood).
pub fn verify_request_signature(
    signing_secret: &str,
    timestamp_header: &str,
    signature_header: &str,
    body: &[u8],
    now_unix_secs: i64,
) -> SignatureCheck {
    if signing_secret.is_empty() {
        return SignatureCheck::Malformed("signing_secret is empty");
    }
    let Ok(ts) = timestamp_header.parse::<i64>() else {
        return SignatureCheck::Malformed("x-slack-request-timestamp not an integer");
    };
    let delta = now_unix_secs.saturating_sub(ts).abs();
    if delta > MAX_SIGNATURE_AGE_SECS {
        return SignatureCheck::Stale;
    }
    let Some(sig_hex) = signature_header.strip_prefix("v0=") else {
        return SignatureCheck::Malformed("x-slack-signature missing v0= prefix");
    };
    let Ok(sig_bytes) = hex::decode(sig_hex) else {
        return SignatureCheck::Malformed("x-slack-signature hex decode failed");
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(signing_secret.as_bytes()) else {
        return SignatureCheck::Malformed("hmac key init failed");
    };
    mac.update(b"v0:");
    mac.update(timestamp_header.as_bytes());
    mac.update(b":");
    mac.update(body);
    match mac.verify_slice(&sig_bytes) {
        Ok(()) => SignatureCheck::Valid,
        Err(_) => SignatureCheck::Mismatch,
    }
}

/// Parsed action lifted from a Block Kit `block_actions`
/// interaction payload.
#[derive(Debug, PartialEq, Eq)]
pub struct InteractionAction {
    /// Approval id the button referenced.
    pub approval_id: String,
    /// `approved` or `rejected` (the wire vocabulary the
    /// coordinator's `approval.record_decision` expects).
    pub decision: &'static str,
    /// Slack user id of the operator who clicked.
    pub user_id: String,
    /// Slack username at click time (Slack always includes
    /// this on `block_actions` payloads).
    pub username: String,
}

/// Errors parsing the Slack interactivity payload.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InteractionParseError {
    /// Form-encoded body did not carry a `payload=` field.
    #[error("slack interaction: missing payload field")]
    MissingPayload,
    /// `payload` value did not URL-decode to valid UTF-8.
    #[error("slack interaction: percent-decode failed: {0}")]
    BadUrlEncoding(String),
    /// `payload` JSON did not parse.
    #[error("slack interaction: JSON parse failed: {0}")]
    BadJson(String),
    /// JSON parsed but the shape isn't a `block_actions`
    /// interaction (e.g. a different interactivity type, or
    /// `actions` is empty).
    #[error("slack interaction: not a block_actions payload")]
    NotBlockActions,
    /// Button `value` did not start with `approve:` or
    /// `deny:`.
    #[error("slack interaction: action value not approve/deny: {0}")]
    UnknownAction(String),
    /// Button value had the right prefix but the trailing
    /// approval id was empty.
    #[error("slack interaction: action value missing approval id")]
    MissingApprovalId,
}

/// Parse the `application/x-www-form-urlencoded` body Slack
/// sends to the interactivity endpoint. Returns the lifted
/// [`InteractionAction`] on success.
pub fn parse_interaction_payload(body: &[u8]) -> Result<InteractionAction, InteractionParseError> {
    let body_str = std::str::from_utf8(body)
        .map_err(|e| InteractionParseError::BadUrlEncoding(e.to_string()))?;
    let payload_raw = body_str
        .split('&')
        .find_map(|kv| kv.strip_prefix("payload="))
        .ok_or(InteractionParseError::MissingPayload)?;
    let payload = percent_decode_form(payload_raw)
        .map_err(|e| InteractionParseError::BadUrlEncoding(e.to_string()))?;
    let v: serde_json::Value = serde_json::from_str(&payload)
        .map_err(|e| InteractionParseError::BadJson(e.to_string()))?;
    if v.get("type").and_then(|t| t.as_str()) != Some("block_actions") {
        return Err(InteractionParseError::NotBlockActions);
    }
    let action = v
        .get("actions")
        .and_then(|a| a.as_array())
        .and_then(|arr| arr.first())
        .ok_or(InteractionParseError::NotBlockActions)?;
    let value = action
        .get("value")
        .and_then(|s| s.as_str())
        .unwrap_or_default();
    let (decision, approval_id) = if let Some(id) = value.strip_prefix(APPROVE_VALUE_PREFIX) {
        ("approved", id)
    } else if let Some(id) = value.strip_prefix(DENY_VALUE_PREFIX) {
        ("rejected", id)
    } else {
        return Err(InteractionParseError::UnknownAction(value.to_string()));
    };
    if approval_id.is_empty() {
        return Err(InteractionParseError::MissingApprovalId);
    }
    let user = v.get("user");
    let user_id = user
        .and_then(|u| u.get("id"))
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    let username = user
        .and_then(|u| u.get("username").or(u.get("name")))
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .to_string();
    Ok(InteractionAction {
        approval_id: approval_id.to_string(),
        decision,
        user_id,
        username,
    })
}

/// Tiny `application/x-www-form-urlencoded` decoder. The
/// Slack interactivity body is always small (single
/// `payload=` field) so we do not pull `percent-encoding`
/// just for this one call site.
fn percent_decode_form(input: &str) -> Result<String, String> {
    let mut out = Vec::with_capacity(input.len());
    let mut bytes = input.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let h = bytes.next().ok_or("truncated % escape")?;
                let l = bytes.next().ok_or("truncated % escape")?;
                let hi = from_hex(h)?;
                let lo = from_hex(l)?;
                out.push(hi * 16 + lo);
            }
            _ => out.push(b),
        }
    }
    String::from_utf8(out).map_err(|e| e.to_string())
}

fn from_hex(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("bad hex digit {b:#x}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SlackApiError;
    use crate::mock::MockSlackApi;

    fn fixture_request(id: &str) -> ApprovalRequest {
        ApprovalRequest {
            approval_id: id.into(),
            agent_name: "finance_alice".into(),
            capability: "tool.stripe.charge".into(),
            request_summary: "charge $100 to customer Bob".into(),
            session_id: "sess-7".into(),
            authorized_approvers: Vec::new(),
        }
    }

    #[test]
    fn blocks_contain_section_then_actions_with_two_buttons() {
        let req = fixture_request("abc-123");
        let blocks = SlackChannelDispatch::build_blocks(&req, false);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "section");
        assert_eq!(blocks[1]["type"], "actions");
        assert_eq!(blocks[1]["block_id"], "approval:abc-123");
        let elements = blocks[1]["elements"].as_array().unwrap();
        assert_eq!(elements.len(), 2);
        assert_eq!(elements[0]["action_id"], "approval_approve");
        assert_eq!(elements[0]["value"], "approve:abc-123");
        assert_eq!(elements[0]["style"], "primary");
        assert_eq!(elements[1]["action_id"], "approval_deny");
        assert_eq!(elements[1]["value"], "deny:abc-123");
        assert_eq!(elements[1]["style"], "danger");
    }

    #[test]
    fn blocks_use_escalated_heading_when_flag_is_true() {
        let req = fixture_request("xyz");
        let blocks = SlackChannelDispatch::build_blocks(&req, true);
        let section_text = blocks[0]["text"]["text"].as_str().unwrap();
        assert!(section_text.contains("ESCALATED"));
        assert!(!section_text.contains(":lock:"));
    }

    #[test]
    fn fallback_text_includes_agent_and_approval_id() {
        let req = fixture_request("a1");
        let s = SlackChannelDispatch::render_fallback_text(&req, false);
        assert!(s.contains("finance_alice"));
        assert!(s.contains("tool.stripe.charge"));
        assert!(s.contains("approval_id=a1"));
    }

    #[tokio::test]
    async fn send_posts_chat_message_with_blocks_and_fallback_text() {
        let mock = Arc::new(MockSlackApi::new());
        let dispatch = SlackChannelDispatch::new(mock.clone(), "C012345".into());
        let req = fixture_request("a1");
        dispatch.send(&req, false).await.expect("send succeeds");
        let sent = mock.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].channel_id, "C012345");
        assert!(sent[0].text.contains("Approval Required"));
        assert_eq!(sent[0].blocks.len(), 2);
        assert_eq!(sent[0].blocks[1]["block_id"], "approval:a1");
    }

    #[tokio::test]
    async fn send_surfaces_transport_failure() {
        let mock = Arc::new(MockSlackApi::new());
        mock.fail_next_send(SlackApiError::Transient("HTTP 502".into()));
        let dispatch = SlackChannelDispatch::new(mock.clone(), "C012345".into());
        let req = fixture_request("a2");
        let err = dispatch.send(&req, false).await.unwrap_err();
        match err {
            ChannelDispatchError::Transport(msg) => {
                assert!(msg.contains("HTTP 502"), "got: {msg}");
            }
            other => panic!("expected Transport, got {other:?}"),
        }
        assert!(mock.sent_messages().is_empty());
    }

    #[tokio::test]
    async fn empty_channel_short_circuits_with_disabled() {
        let mock = Arc::new(MockSlackApi::new());
        let dispatch = SlackChannelDispatch::new(mock.clone(), String::new());
        let req = fixture_request("a3");
        let err = dispatch.send(&req, false).await.unwrap_err();
        match err {
            ChannelDispatchError::Disabled(name) => assert_eq!(name, "slack"),
            other => panic!("expected Disabled, got {other:?}"),
        }
    }

    // ── signature verification ────────────────────────────

    /// Reference HMAC for the Slack-documented sample
    /// (timestamp + body). Computed live (`Hmac::<Sha256>`
    /// is deterministic) so the test fails loud if we
    /// accidentally change the basestring shape.
    fn build_signature(secret: &str, ts: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(b"v0:");
        mac.update(ts.as_bytes());
        mac.update(b":");
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        format!("v0={}", hex::encode(bytes))
    }

    #[test]
    fn valid_signature_matches_within_freshness_window() {
        let secret = "test-signing-secret";
        let ts = "1700000000";
        let body = b"payload=%7B%22type%22%3A%22block_actions%22%7D";
        let sig = build_signature(secret, ts, body);
        let r = verify_request_signature(secret, ts, &sig, body, 1700000060);
        assert_eq!(r, SignatureCheck::Valid);
    }

    #[test]
    fn stale_signature_rejected() {
        let secret = "test-signing-secret";
        let ts = "1700000000";
        let body = b"payload=...";
        let sig = build_signature(secret, ts, body);
        // 10 minutes later — outside the 5 min window.
        let r = verify_request_signature(secret, ts, &sig, body, 1700000000 + 600);
        assert_eq!(r, SignatureCheck::Stale);
    }

    #[test]
    fn mismatched_signature_rejected() {
        let secret = "test-signing-secret";
        let ts = "1700000000";
        let body = b"payload=tampered";
        let sig = build_signature(secret, ts, body);
        // Tamper with the body without updating the signature.
        let r = verify_request_signature(secret, ts, &sig, b"payload=other", 1700000060);
        assert_eq!(r, SignatureCheck::Mismatch);
    }

    #[test]
    fn malformed_signature_header_rejected() {
        let r = verify_request_signature(
            "secret",
            "1700000000",
            "no-prefix-here",
            b"payload=x",
            1700000060,
        );
        assert!(matches!(r, SignatureCheck::Malformed(_)));
    }

    #[test]
    fn non_integer_timestamp_rejected() {
        let r = verify_request_signature(
            "secret",
            "not-a-timestamp",
            "v0=deadbeef",
            b"payload=x",
            1700000060,
        );
        assert!(matches!(r, SignatureCheck::Malformed(_)));
    }

    #[test]
    fn empty_signing_secret_rejected() {
        let r = verify_request_signature("", "1700000000", "v0=deadbeef", b"payload=x", 1700000060);
        assert!(matches!(r, SignatureCheck::Malformed(_)));
    }

    // ── interaction payload parsing ───────────────────────

    fn block_actions_payload(value: &str, user_id: &str, username: &str) -> String {
        let json = serde_json::json!({
            "type": "block_actions",
            "user": { "id": user_id, "username": username },
            "actions": [
                { "action_id": "approval_approve", "value": value }
            ]
        });
        let json_text = json.to_string();
        // Form-encode (percent-encode the JSON value).
        let mut out = String::from("payload=");
        for b in json_text.bytes() {
            match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char);
                }
                _ => out.push_str(&format!("%{:02X}", b)),
            }
        }
        out
    }

    #[test]
    fn parse_approve_payload_yields_approved_decision() {
        let body = block_actions_payload("approve:abc-123", "U12345", "alice");
        let action = parse_interaction_payload(body.as_bytes()).unwrap();
        assert_eq!(action.approval_id, "abc-123");
        assert_eq!(action.decision, "approved");
        assert_eq!(action.user_id, "U12345");
        assert_eq!(action.username, "alice");
    }

    #[test]
    fn parse_deny_payload_yields_rejected_decision() {
        let body = block_actions_payload("deny:abc-123", "U67890", "bob");
        let action = parse_interaction_payload(body.as_bytes()).unwrap();
        assert_eq!(action.approval_id, "abc-123");
        assert_eq!(action.decision, "rejected");
    }

    #[test]
    fn parse_unknown_action_value_rejected() {
        let body = block_actions_payload("shrug:abc-123", "U", "u");
        let err = parse_interaction_payload(body.as_bytes()).unwrap_err();
        assert!(matches!(
            err,
            InteractionParseError::UnknownAction(ref v) if v == "shrug:abc-123"
        ));
    }

    #[test]
    fn parse_empty_approval_id_rejected() {
        let body = block_actions_payload("approve:", "U", "u");
        let err = parse_interaction_payload(body.as_bytes()).unwrap_err();
        assert_eq!(err, InteractionParseError::MissingApprovalId);
    }

    #[test]
    fn parse_missing_payload_field_rejected() {
        let err = parse_interaction_payload(b"other=stuff").unwrap_err();
        assert_eq!(err, InteractionParseError::MissingPayload);
    }

    #[test]
    fn parse_non_block_actions_type_rejected() {
        let json = serde_json::json!({
            "type": "shortcut",
            "actions": []
        });
        let mut body = String::from("payload=");
        for b in json.to_string().bytes() {
            match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    body.push(b as char);
                }
                _ => body.push_str(&format!("%{:02X}", b)),
            }
        }
        let err = parse_interaction_payload(body.as_bytes()).unwrap_err();
        assert_eq!(err, InteractionParseError::NotBlockActions);
    }

    #[test]
    fn parse_truncated_percent_escape_rejected() {
        // `%2` is incomplete.
        let err = parse_interaction_payload(b"payload=%2").unwrap_err();
        assert!(matches!(err, InteractionParseError::BadUrlEncoding(_)));
    }
}
