//! `DiscordChannelDispatch` — wire-real implementation of
//! [`SingleChannelDispatch`] for Discord.
//!
//! Renders an approval request as a Discord message with an
//! Action Row carrying Approve / Deny buttons. The buttons'
//! `custom_id` encodes the approval id; Discord echoes the
//! `custom_id` back on the interaction the operator's click
//! produces, the bridge verifies the
//! `X-Signature-Ed25519` + `X-Signature-Timestamp` headers,
//! and routes the decision to the coordinator's
//! `approval.record_decision` cap.
//!
//! This module also exposes the wire-level helpers the bridge
//! consumes for the inbound interaction route:
//!
//! - [`verify_interaction_signature`] — constant-time Ed25519
//!   verification of `X-Signature-Ed25519` against the operator-
//!   supplied application public key.
//! - [`parse_interaction_payload`] — classifies the interaction
//!   type and, for `MESSAGE_COMPONENT` (button) interactions,
//!   returns the lifted [`InteractionAction`].
//! - [`pong_response`] / [`ack_response`] — pre-rendered JSON
//!   bodies the bridge returns to Discord (PING ⇒ PONG;
//!   button click ⇒ ephemeral acknowledgement so the operator
//!   sees their decision was recorded).

use std::sync::Arc;

use async_trait::async_trait;
use ed25519_dalek::{Signature, VerifyingKey};

use relix_core::approval::{ApprovalRequest, ChannelDispatchError, SingleChannelDispatch};

use crate::{DiscordApi, OutgoingMessage};

/// Discord component type tag for Action Row containers.
pub const COMPONENT_TYPE_ACTION_ROW: u64 = 1;
/// Discord component type tag for Button elements.
pub const COMPONENT_TYPE_BUTTON: u64 = 2;

/// Discord button style: `primary` (blurple). Used for the
/// Approve button.
pub const BUTTON_STYLE_PRIMARY: u64 = 1;
/// Discord button style: `danger` (red). Used for the Deny
/// button.
pub const BUTTON_STYLE_DANGER: u64 = 4;

/// Discord interaction type for the verification PING — sent
/// when an operator pastes the Interactions Endpoint URL into
/// the Discord Developer Portal.
pub const INTERACTION_TYPE_PING: u64 = 1;
/// Discord interaction type for application commands. Bridge
/// short-circuits these with an ACK; we don't model commands
/// here.
pub const INTERACTION_TYPE_APPLICATION_COMMAND: u64 = 2;
/// Discord interaction type for message components — buttons,
/// select menus, etc. Approval clicks land here.
pub const INTERACTION_TYPE_MESSAGE_COMPONENT: u64 = 3;

/// Discord interaction response type for PONG (reply to PING).
pub const RESPONSE_TYPE_PONG: u64 = 1;
/// Discord interaction response type for "deferred update
/// message" — sent in reply to a button click when the bridge
/// wants to acknowledge without showing anything new to the
/// operator's chat history.
pub const RESPONSE_TYPE_DEFERRED_UPDATE_MESSAGE: u64 = 6;
/// Discord interaction response type for "channel message with
/// source" — used when the bridge wants to confirm the decision
/// with an ephemeral message to the operator.
pub const RESPONSE_TYPE_CHANNEL_MESSAGE_WITH_SOURCE: u64 = 4;

/// `MESSAGE_FLAGS_EPHEMERAL` — bit flag on
/// `response.data.flags` that scopes the response message to
/// the clicking operator (nobody else in the channel sees it).
pub const MESSAGE_FLAG_EPHEMERAL: u64 = 1 << 6;

/// Button `custom_id` prefix carried in the Approve button.
/// The bridge's interaction handler matches against these when
/// parsing the payload.
pub const APPROVE_CUSTOM_ID_PREFIX: &str = "approve:";
/// Button `custom_id` prefix for the Deny button.
pub const DENY_CUSTOM_ID_PREFIX: &str = "deny:";

/// Wire-real per-channel dispatcher. Holds the [`DiscordApi`]
/// handle behind an [`Arc`] so the controller's startup wires
/// one [`crate::LiveDiscordApi`] and hands it to the receive
/// loop AND the approval pipeline. `channel_id` is the
/// snowflake (string) of the channel approval notifications
/// post into.
#[derive(Clone)]
pub struct DiscordChannelDispatch {
    api: Arc<dyn DiscordApi>,
    channel_id: String,
}

impl DiscordChannelDispatch {
    /// Construct a new dispatcher. Caller has already validated
    /// the channel id is non-empty and the bot has been invited
    /// to the channel with the `Send Messages` and
    /// `Use Application Commands` permissions.
    pub fn new(api: Arc<dyn DiscordApi>, channel_id: String) -> Self {
        Self { api, channel_id }
    }

    /// Build the Discord `components` array for one approval
    /// request — a single Action Row carrying Approve + Deny
    /// buttons. Exposed for testing.
    pub fn build_components(approval_id: &str) -> Vec<serde_json::Value> {
        vec![serde_json::json!({
            "type": COMPONENT_TYPE_ACTION_ROW,
            "components": [
                {
                    "type": COMPONENT_TYPE_BUTTON,
                    "style": BUTTON_STYLE_PRIMARY,
                    "label": "Approve",
                    "custom_id": format!("{APPROVE_CUSTOM_ID_PREFIX}{approval_id}"),
                },
                {
                    "type": COMPONENT_TYPE_BUTTON,
                    "style": BUTTON_STYLE_DANGER,
                    "label": "Deny",
                    "custom_id": format!("{DENY_CUSTOM_ID_PREFIX}{approval_id}"),
                },
            ],
        })]
    }

    /// Render the operator-facing approval body. Exposed for
    /// testing. Discord renders markdown inline so we use the
    /// same `**bold**` / `` `code` `` shapes Slack does, minus
    /// Slack-specific emoji shortcodes.
    pub fn render_body(request: &ApprovalRequest, is_escalation: bool) -> String {
        let heading = if is_escalation {
            "🚨 **ESCALATED Approval Required**"
        } else {
            "🔐 **Approval Required**"
        };
        format!(
            "{heading}\n\n**Agent:** {agent}\n**Action:** `{capability}`\n\
             **Request:** {summary}\n**Session:** `{session}`\n\
             **Approval ID:** `{id}`",
            agent = request.agent_name,
            capability = request.capability,
            summary = request.request_summary,
            session = request.session_id,
            id = request.approval_id,
        )
    }
}

#[async_trait]
impl SingleChannelDispatch for DiscordChannelDispatch {
    async fn send(
        &self,
        request: &ApprovalRequest,
        is_escalation: bool,
    ) -> Result<(), ChannelDispatchError> {
        if self.channel_id.is_empty() {
            return Err(ChannelDispatchError::Disabled("discord".into()));
        }
        let out = OutgoingMessage {
            channel_id: self.channel_id.clone(),
            reply_to_message_id: String::new(),
            content: Self::render_body(request, is_escalation),
            components: Self::build_components(&request.approval_id),
        };
        self.api
            .send_message(&out)
            .await
            .map_err(|e| ChannelDispatchError::Transport(format!("discord: {e}")))
    }
}

// ── inbound interaction verification ─────────────────────

/// Result of verifying a Discord interaction signature.
#[derive(Debug, PartialEq, Eq)]
pub enum SignatureCheck {
    /// Signature parsed AND the Ed25519 verify matched.
    Valid,
    /// Header / public key is missing or has the wrong shape.
    Malformed(&'static str),
    /// Signature decoded but the verify failed.
    Mismatch,
}

/// Verify the Discord interaction `X-Signature-Ed25519`
/// against the body. Discord signs the concatenation of the
/// `X-Signature-Timestamp` header bytes and the raw request
/// body bytes with the application's Ed25519 private key; the
/// public key is published in the Discord Developer Portal and
/// operators paste it into the bridge's
/// `RELIX_BRIDGE_DISCORD_PUBLIC_KEY` env var.
///
/// `verify_strict` returns `Ok(())` only when the signature is
/// the canonical encoding of a valid Ed25519 signature for the
/// given message — no malleability. Constant-time at the
/// underlying primitive level.
pub fn verify_interaction_signature(
    public_key_hex: &str,
    timestamp_header: &str,
    signature_header: &str,
    body: &[u8],
) -> SignatureCheck {
    if public_key_hex.is_empty() {
        return SignatureCheck::Malformed("public_key_hex is empty");
    }
    let Ok(pk_bytes) = hex::decode(public_key_hex) else {
        return SignatureCheck::Malformed("public_key_hex: hex decode failed");
    };
    let Ok(pk_arr): Result<[u8; 32], _> = pk_bytes.as_slice().try_into() else {
        return SignatureCheck::Malformed("public_key_hex: expected 32 bytes");
    };
    let Ok(pk) = VerifyingKey::from_bytes(&pk_arr) else {
        return SignatureCheck::Malformed("public_key_hex: not a valid Ed25519 point");
    };
    let Ok(sig_bytes) = hex::decode(signature_header) else {
        return SignatureCheck::Malformed("x-signature-ed25519: hex decode failed");
    };
    let Ok(sig_arr): Result<[u8; 64], _> = sig_bytes.as_slice().try_into() else {
        return SignatureCheck::Malformed("x-signature-ed25519: expected 64 bytes");
    };
    let sig = Signature::from_bytes(&sig_arr);
    let mut msg = Vec::with_capacity(timestamp_header.len() + body.len());
    msg.extend_from_slice(timestamp_header.as_bytes());
    msg.extend_from_slice(body);
    match pk.verify_strict(&msg, &sig) {
        Ok(()) => SignatureCheck::Valid,
        Err(_) => SignatureCheck::Mismatch,
    }
}

/// Parsed action lifted from a Discord `MESSAGE_COMPONENT`
/// interaction payload.
#[derive(Debug, PartialEq, Eq)]
pub struct InteractionAction {
    /// Approval id the button referenced.
    pub approval_id: String,
    /// `approved` or `rejected` — wire vocabulary the
    /// coordinator's `approval.record_decision` expects.
    pub decision: &'static str,
    /// Discord user snowflake of the operator who clicked.
    pub user_id: String,
    /// Operator-visible username at click time (Discord
    /// includes this on every interaction).
    pub username: String,
}

/// Discriminated outcome of parsing a Discord interaction
/// body — the bridge needs to distinguish the verification
/// PING (which must be PONGed) from a real component click.
#[derive(Debug, PartialEq, Eq)]
pub enum InteractionKind {
    /// `type == 1` — Discord verification PING. Bridge replies
    /// with [`pong_response`].
    Ping,
    /// `type == 3` — message component (button) click. Bridge
    /// records the decision and replies with [`ack_response`].
    Component(InteractionAction),
    /// Any other interaction type — currently
    /// `APPLICATION_COMMAND` (slash command) or
    /// `MODAL_SUBMIT`. Bridge logs and returns the deferred
    /// update message response.
    Other(u64),
}

/// Errors parsing the Discord interaction payload.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InteractionParseError {
    /// JSON did not parse.
    #[error("discord interaction: JSON parse failed: {0}")]
    BadJson(String),
    /// `type` field is missing or not a number.
    #[error("discord interaction: missing interaction type")]
    MissingType,
    /// `data.custom_id` is absent or empty on a component
    /// interaction.
    #[error("discord interaction: missing custom_id")]
    MissingCustomId,
    /// Button `custom_id` did not start with `approve:` or
    /// `deny:`.
    #[error("discord interaction: custom_id not approve/deny: {0}")]
    UnknownAction(String),
    /// Custom id had the right prefix but trailing approval id
    /// was empty.
    #[error("discord interaction: action missing approval id")]
    MissingApprovalId,
}

/// Parse the raw Discord interaction body. Returns the
/// classified [`InteractionKind`] so the bridge can route
/// PINGs vs component clicks distinctly.
pub fn parse_interaction_payload(body: &[u8]) -> Result<InteractionKind, InteractionParseError> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| InteractionParseError::BadJson(e.to_string()))?;
    let ty = v
        .get("type")
        .and_then(|t| t.as_u64())
        .ok_or(InteractionParseError::MissingType)?;
    match ty {
        INTERACTION_TYPE_PING => Ok(InteractionKind::Ping),
        INTERACTION_TYPE_MESSAGE_COMPONENT => {
            let custom_id = v
                .get("data")
                .and_then(|d| d.get("custom_id"))
                .and_then(|s| s.as_str())
                .unwrap_or_default();
            if custom_id.is_empty() {
                return Err(InteractionParseError::MissingCustomId);
            }
            let (decision, approval_id) =
                if let Some(id) = custom_id.strip_prefix(APPROVE_CUSTOM_ID_PREFIX) {
                    ("approved", id)
                } else if let Some(id) = custom_id.strip_prefix(DENY_CUSTOM_ID_PREFIX) {
                    ("rejected", id)
                } else {
                    return Err(InteractionParseError::UnknownAction(custom_id.to_string()));
                };
            if approval_id.is_empty() {
                return Err(InteractionParseError::MissingApprovalId);
            }
            // Discord puts the user object under `member.user`
            // in guild interactions and under `user` directly
            // in DM interactions. Try both.
            let user_obj = v
                .get("member")
                .and_then(|m| m.get("user"))
                .or_else(|| v.get("user"));
            let user_id = user_obj
                .and_then(|u| u.get("id"))
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string();
            let username = user_obj
                .and_then(|u| u.get("username").or(u.get("global_name")))
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string();
            Ok(InteractionKind::Component(InteractionAction {
                approval_id: approval_id.to_string(),
                decision,
                user_id,
                username,
            }))
        }
        _ => Ok(InteractionKind::Other(ty)),
    }
}

/// Pre-rendered JSON body for the Discord verification PONG.
pub fn pong_response() -> serde_json::Value {
    serde_json::json!({ "type": RESPONSE_TYPE_PONG })
}

/// Pre-rendered ephemeral acknowledgement response for a
/// component click. `content` is the message Discord renders
/// in the operator's chat (visible only to them).
pub fn ack_response(content: &str) -> serde_json::Value {
    serde_json::json!({
        "type": RESPONSE_TYPE_CHANNEL_MESSAGE_WITH_SOURCE,
        "data": {
            "content": content,
            "flags": MESSAGE_FLAG_EPHEMERAL,
        }
    })
}

/// Pre-rendered deferred response for non-component, non-PING
/// interactions. Tells the Discord client "I saw your
/// interaction; no visible reply needed."
pub fn deferred_update_response() -> serde_json::Value {
    serde_json::json!({ "type": RESPONSE_TYPE_DEFERRED_UPDATE_MESSAGE })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DiscordApiError;
    use crate::mock::MockDiscordApi;
    use ed25519_dalek::{Signer, SigningKey};

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
    fn components_are_one_action_row_with_two_buttons() {
        let c = DiscordChannelDispatch::build_components("abc-123");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["type"], COMPONENT_TYPE_ACTION_ROW);
        let buttons = c[0]["components"].as_array().unwrap();
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0]["type"], COMPONENT_TYPE_BUTTON);
        assert_eq!(buttons[0]["style"], BUTTON_STYLE_PRIMARY);
        assert_eq!(buttons[0]["label"], "Approve");
        assert_eq!(buttons[0]["custom_id"], "approve:abc-123");
        assert_eq!(buttons[1]["style"], BUTTON_STYLE_DANGER);
        assert_eq!(buttons[1]["label"], "Deny");
        assert_eq!(buttons[1]["custom_id"], "deny:abc-123");
    }

    #[test]
    fn body_carries_every_request_field_and_initial_heading() {
        let req = fixture_request("abc-123");
        let body = DiscordChannelDispatch::render_body(&req, false);
        assert!(body.contains("🔐 **Approval Required**"));
        assert!(!body.contains("ESCALATED"));
        assert!(body.contains("**Agent:** finance_alice"));
        assert!(body.contains("**Action:** `tool.stripe.charge`"));
        assert!(body.contains("**Approval ID:** `abc-123`"));
    }

    #[test]
    fn body_uses_escalated_heading_when_flag_is_true() {
        let req = fixture_request("xyz");
        let body = DiscordChannelDispatch::render_body(&req, true);
        assert!(body.contains("🚨 **ESCALATED Approval Required**"));
        assert!(!body.contains("🔐 **Approval Required**"));
    }

    #[tokio::test]
    async fn send_posts_message_with_components_and_content() {
        let mock = Arc::new(MockDiscordApi::new());
        let dispatch = DiscordChannelDispatch::new(mock.clone(), "100".into());
        let req = fixture_request("a1");
        dispatch.send(&req, false).await.expect("send succeeds");
        let sent = mock.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].channel_id, "100");
        assert!(sent[0].content.contains("Approval Required"));
        assert_eq!(sent[0].components.len(), 1);
        let buttons = sent[0].components[0]["components"].as_array().unwrap();
        assert_eq!(buttons[0]["custom_id"], "approve:a1");
        assert_eq!(buttons[1]["custom_id"], "deny:a1");
    }

    #[tokio::test]
    async fn send_surfaces_transport_failure() {
        let mock = Arc::new(MockDiscordApi::new());
        mock.fail_next_send(DiscordApiError::Transient("HTTP 502".into()));
        let dispatch = DiscordChannelDispatch::new(mock.clone(), "100".into());
        let err = dispatch
            .send(&fixture_request("a2"), false)
            .await
            .unwrap_err();
        match err {
            ChannelDispatchError::Transport(msg) => assert!(msg.contains("HTTP 502")),
            other => panic!("expected Transport, got {other:?}"),
        }
        assert!(mock.sent_messages().is_empty());
    }

    #[tokio::test]
    async fn empty_channel_short_circuits_with_disabled() {
        let mock = Arc::new(MockDiscordApi::new());
        let dispatch = DiscordChannelDispatch::new(mock.clone(), String::new());
        let err = dispatch
            .send(&fixture_request("a3"), false)
            .await
            .unwrap_err();
        match err {
            ChannelDispatchError::Disabled(name) => assert_eq!(name, "discord"),
            other => panic!("expected Disabled, got {other:?}"),
        }
    }

    // ── signature verification ────────────────────────────

    /// Build a real Ed25519 signature for the given payload —
    /// exercises the verify path against a freshly-generated
    /// keypair so tests stay deterministic without storing
    /// secret material in the repo.
    fn fixture_signed(body: &[u8], timestamp: &str) -> (String, String) {
        // Deterministic signing key — the test runs are
        // reproducible regardless of host RNG.
        let secret = [42u8; 32];
        let sk = SigningKey::from_bytes(&secret);
        let pk_hex = hex::encode(sk.verifying_key().to_bytes());
        let mut msg = Vec::with_capacity(timestamp.len() + body.len());
        msg.extend_from_slice(timestamp.as_bytes());
        msg.extend_from_slice(body);
        let sig = sk.sign(&msg);
        let sig_hex = hex::encode(sig.to_bytes());
        (pk_hex, sig_hex)
    }

    #[test]
    fn valid_signature_passes() {
        let ts = "1700000000";
        let body = br#"{"type":1}"#;
        let (pk, sig) = fixture_signed(body, ts);
        let r = verify_interaction_signature(&pk, ts, &sig, body);
        assert_eq!(r, SignatureCheck::Valid);
    }

    #[test]
    fn tampered_body_fails_verification() {
        let ts = "1700000000";
        let body = br#"{"type":3}"#;
        let (pk, sig) = fixture_signed(body, ts);
        // Flip a byte in the body.
        let tampered = br#"{"type":2}"#;
        let r = verify_interaction_signature(&pk, ts, &sig, tampered);
        assert_eq!(r, SignatureCheck::Mismatch);
    }

    #[test]
    fn tampered_timestamp_fails_verification() {
        let body = br#"{"type":3}"#;
        let (pk, sig) = fixture_signed(body, "1700000000");
        let r = verify_interaction_signature(&pk, "1700000001", &sig, body);
        assert_eq!(r, SignatureCheck::Mismatch);
    }

    #[test]
    fn empty_public_key_is_malformed() {
        let r = verify_interaction_signature("", "1700000000", "deadbeef", b"{}");
        assert!(matches!(r, SignatureCheck::Malformed(_)));
    }

    #[test]
    fn non_hex_signature_is_malformed() {
        let (pk, _) = fixture_signed(b"x", "ts");
        let r = verify_interaction_signature(&pk, "ts", "not-hex", b"x");
        assert!(matches!(r, SignatureCheck::Malformed(_)));
    }

    #[test]
    fn wrong_length_signature_is_malformed() {
        let (pk, _) = fixture_signed(b"x", "ts");
        // 32 bytes hex-encoded — too short for an Ed25519 sig
        // (which is 64 bytes).
        let r = verify_interaction_signature(&pk, "ts", &"ab".repeat(32), b"x");
        assert!(matches!(r, SignatureCheck::Malformed(_)));
    }

    // ── payload parsing ───────────────────────────────────

    #[test]
    fn parse_ping_classifies_as_ping() {
        let body = br#"{"type":1}"#;
        let k = parse_interaction_payload(body).unwrap();
        assert_eq!(k, InteractionKind::Ping);
    }

    #[test]
    fn parse_approve_button_yields_approved_action() {
        let body = serde_json::json!({
            "type": 3,
            "data": { "custom_id": "approve:abc-123" },
            "member": { "user": { "id": "U12345", "username": "alice" } }
        });
        let k = parse_interaction_payload(body.to_string().as_bytes()).unwrap();
        match k {
            InteractionKind::Component(a) => {
                assert_eq!(a.approval_id, "abc-123");
                assert_eq!(a.decision, "approved");
                assert_eq!(a.user_id, "U12345");
                assert_eq!(a.username, "alice");
            }
            other => panic!("expected Component, got {other:?}"),
        }
    }

    #[test]
    fn parse_deny_button_yields_rejected_action() {
        let body = serde_json::json!({
            "type": 3,
            "data": { "custom_id": "deny:abc-123" },
            "user": { "id": "U67890", "username": "bob" }
        });
        let k = parse_interaction_payload(body.to_string().as_bytes()).unwrap();
        match k {
            InteractionKind::Component(a) => {
                assert_eq!(a.decision, "rejected");
                assert_eq!(a.user_id, "U67890");
                assert_eq!(a.username, "bob");
            }
            other => panic!("expected Component, got {other:?}"),
        }
    }

    #[test]
    fn parse_other_interaction_types_surface_as_other() {
        let body = br#"{"type":2}"#;
        let k = parse_interaction_payload(body).unwrap();
        assert_eq!(k, InteractionKind::Other(2));
    }

    #[test]
    fn parse_unknown_custom_id_prefix_rejected() {
        let body = serde_json::json!({
            "type": 3,
            "data": { "custom_id": "shrug:abc" }
        });
        let err = parse_interaction_payload(body.to_string().as_bytes()).unwrap_err();
        assert!(matches!(err, InteractionParseError::UnknownAction(ref v) if v == "shrug:abc"));
    }

    #[test]
    fn parse_empty_approval_id_rejected() {
        let body = serde_json::json!({
            "type": 3,
            "data": { "custom_id": "approve:" }
        });
        let err = parse_interaction_payload(body.to_string().as_bytes()).unwrap_err();
        assert_eq!(err, InteractionParseError::MissingApprovalId);
    }

    #[test]
    fn parse_missing_custom_id_rejected() {
        let body = serde_json::json!({
            "type": 3,
            "data": {}
        });
        let err = parse_interaction_payload(body.to_string().as_bytes()).unwrap_err();
        assert_eq!(err, InteractionParseError::MissingCustomId);
    }

    #[test]
    fn parse_missing_type_rejected() {
        let body = br#"{"data":{}}"#;
        let err = parse_interaction_payload(body).unwrap_err();
        assert_eq!(err, InteractionParseError::MissingType);
    }

    #[test]
    fn parse_malformed_json_rejected() {
        let err = parse_interaction_payload(b"{not json").unwrap_err();
        assert!(matches!(err, InteractionParseError::BadJson(_)));
    }

    #[test]
    fn pong_response_matches_discord_wire_shape() {
        assert_eq!(pong_response(), serde_json::json!({"type": 1}));
    }

    #[test]
    fn ack_response_is_ephemeral_channel_message() {
        let r = ack_response("ok");
        assert_eq!(r["type"], 4);
        assert_eq!(r["data"]["content"], "ok");
        // Bit 6 = ephemeral (1 << 6 = 64).
        assert_eq!(r["data"]["flags"], 64);
    }
}
