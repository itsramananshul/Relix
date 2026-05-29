//! `TelegramChannelDispatch` — wire-real implementation of
//! [`SingleChannelDispatch`] for Telegram.
//!
//! Renders an approval request as a Telegram message with an
//! inline keyboard carrying **Approve** / **Deny** buttons.
//! The buttons' `callback_data` field embeds the approval id
//! using the existing `/approve <id>` / `/deny <id>` slash-
//! command syntax so the controller's command router picks
//! up button presses identically to free-text replies.
//!
//! The dispatcher does NOT format the body with `MarkdownV2`.
//! Telegram's `MarkdownV2` is unforgiving and a stray `*` in
//! an agent name or capability label silently breaks message
//! delivery. Plain text + emoji prefixes is the only safe
//! shape for an operator-facing alert.
//!
//! On error from the Bot API, [`SingleChannelDispatch::send`]
//! returns a [`ChannelDispatchError`]:
//!
//! - [`ChannelDispatchError::Transport`] — the live Bot API
//!   already retried within budget and exhausted attempts.
//!   The runtime treats this as a delivery failure and marks
//!   the row `delivery_failed`.
//! - [`ChannelDispatchError::Disabled`] — surfaced when the
//!   credentials are missing at construction time (we fail
//!   loudly during startup rather than letting the dispatcher
//!   no-op).
//!
//! Tests use [`crate::mock::MockBotApi`] so the production
//! retry / parse code is exercised without HTTPS.

use std::sync::Arc;

use async_trait::async_trait;

use relix_core::approval::{ApprovalRequest, ChannelDispatchError, SingleChannelDispatch};

use crate::messages::{InlineKeyboardButton, InlineKeyboardMarkup};
use crate::{BotApi, OutgoingMessage};

/// Wire-real per-channel dispatcher. Holds the
/// [`BotApi`] handle behind an [`Arc`] so the same client the
/// controller's receive loop uses is shared with the approval
/// pipeline — startup builds one [`crate::LiveBotApi`] and
/// hands a `dyn BotApi` to both consumers.
///
/// `chat_id` is the numeric Telegram chat to post approval
/// notifications into. Operators set this in the
/// `[approval.delivery.channels.telegram]` config block.
#[derive(Clone)]
pub struct TelegramChannelDispatch {
    api: Arc<dyn BotApi>,
    chat_id: i64,
}

impl TelegramChannelDispatch {
    /// Construct a new dispatcher. Caller has already
    /// validated that `chat_id != 0` and that the bot has
    /// permission to post in the chat.
    pub fn new(api: Arc<dyn BotApi>, chat_id: i64) -> Self {
        Self { api, chat_id }
    }

    /// Build the inline keyboard for one approval. Exposed
    /// for testing — production callers go through
    /// [`SingleChannelDispatch::send`].
    pub fn build_keyboard(approval_id: &str) -> InlineKeyboardMarkup {
        InlineKeyboardMarkup {
            inline_keyboard: vec![vec![
                InlineKeyboardButton {
                    text: "✅ Approve".into(),
                    callback_data: format!("/approve {approval_id}"),
                },
                InlineKeyboardButton {
                    text: "❌ Deny".into(),
                    callback_data: format!("/deny {approval_id}"),
                },
            ]],
        }
    }

    /// Render the operator-facing approval body. Exposed for
    /// testing.
    pub fn render_body(request: &ApprovalRequest, is_escalation: bool) -> String {
        let heading = if is_escalation {
            "⚠️ ESCALATED Approval Required"
        } else {
            "🔐 Approval Required"
        };
        format!(
            "{heading}\n\nAgent: {agent}\nAction: {capability}\nRequest: {summary}\nSession: \
             {session}\n\nApproval ID: {id}",
            agent = request.agent_name,
            capability = request.capability,
            summary = request.request_summary,
            session = request.session_id,
            id = request.approval_id,
        )
    }
}

#[async_trait]
impl SingleChannelDispatch for TelegramChannelDispatch {
    async fn send(
        &self,
        request: &ApprovalRequest,
        is_escalation: bool,
    ) -> Result<(), ChannelDispatchError> {
        if self.chat_id == 0 {
            return Err(ChannelDispatchError::Disabled("telegram".into()));
        }
        let out = OutgoingMessage {
            chat_id: self.chat_id,
            reply_to_message_id: 0,
            text: Self::render_body(request, is_escalation),
            parse_mode: None,
            reply_markup: Some(Self::build_keyboard(&request.approval_id)),
        };
        self.api
            .send_message(&out)
            .await
            .map_err(|e| ChannelDispatchError::Transport(format!("telegram: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BotApiError;
    use crate::mock::MockBotApi;

    fn fixture_request(id: &str, escalation_label: &str) -> ApprovalRequest {
        ApprovalRequest {
            approval_id: id.into(),
            agent_name: "finance_alice".into(),
            capability: "tool.stripe.charge".into(),
            request_summary: format!("charge $100 to customer Bob {escalation_label}"),
            session_id: "sess-7".into(),
            authorized_approvers: Vec::new(),
        }
    }

    #[test]
    fn keyboard_has_approve_and_deny_buttons_in_one_row() {
        let kb = TelegramChannelDispatch::build_keyboard("abc-123");
        assert_eq!(kb.inline_keyboard.len(), 1);
        let row = &kb.inline_keyboard[0];
        assert_eq!(row.len(), 2);
        assert!(row[0].text.starts_with("✅"));
        assert_eq!(row[0].callback_data, "/approve abc-123");
        assert!(row[1].text.starts_with("❌"));
        assert_eq!(row[1].callback_data, "/deny abc-123");
    }

    #[test]
    fn body_carries_every_request_field_and_initial_heading() {
        let req = fixture_request("abc-123", "initial");
        let body = TelegramChannelDispatch::render_body(&req, false);
        assert!(body.contains("🔐 Approval Required"));
        assert!(!body.contains("ESCALATED"));
        assert!(body.contains("Agent: finance_alice"));
        assert!(body.contains("Action: tool.stripe.charge"));
        assert!(body.contains("Request: charge $100 to customer Bob initial"));
        assert!(body.contains("Session: sess-7"));
        assert!(body.contains("Approval ID: abc-123"));
    }

    #[test]
    fn body_uses_escalated_heading_when_flag_is_true() {
        let req = fixture_request("xyz-9", "escalated");
        let body = TelegramChannelDispatch::render_body(&req, true);
        assert!(body.contains("⚠️ ESCALATED Approval Required"));
        assert!(!body.contains("🔐 Approval Required"));
    }

    #[tokio::test]
    async fn send_posts_message_with_inline_keyboard() {
        let mock = Arc::new(MockBotApi::new());
        let dispatch = TelegramChannelDispatch::new(mock.clone(), 999);
        let req = fixture_request("a1", "initial");
        dispatch.send(&req, false).await.expect("send succeeds");
        let sent = mock.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].chat_id, 999);
        assert!(sent[0].text.contains("🔐 Approval Required"));
        let kb = sent[0]
            .reply_markup
            .as_ref()
            .expect("reply_markup attached")
            .inline_keyboard
            .clone();
        assert_eq!(kb.len(), 1);
        assert_eq!(kb[0][0].callback_data, "/approve a1");
        assert_eq!(kb[0][1].callback_data, "/deny a1");
    }

    #[tokio::test]
    async fn send_escalation_uses_escalated_heading() {
        let mock = Arc::new(MockBotApi::new());
        let dispatch = TelegramChannelDispatch::new(mock.clone(), 999);
        let req = fixture_request("a2", "esc");
        dispatch.send(&req, true).await.expect("send succeeds");
        let sent = mock.sent_messages();
        assert!(sent[0].text.contains("⚠️ ESCALATED Approval Required"));
    }

    #[tokio::test]
    async fn send_surfaces_transient_failure_as_dispatch_error() {
        let mock = Arc::new(MockBotApi::new());
        mock.fail_next_send(BotApiError::Transient("HTTP 502".into()));
        let dispatch = TelegramChannelDispatch::new(mock.clone(), 999);
        let req = fixture_request("a3", "fail");
        let err = dispatch.send(&req, false).await.unwrap_err();
        match err {
            ChannelDispatchError::Transport(msg) => {
                assert!(msg.contains("HTTP 502"), "got: {msg}");
            }
            other => panic!("expected Transport, got {other:?}"),
        }
        // Nothing landed in the mock outbound queue.
        assert!(mock.sent_messages().is_empty());
    }

    #[tokio::test]
    async fn zero_chat_id_short_circuits_with_disabled() {
        let mock = Arc::new(MockBotApi::new());
        let dispatch = TelegramChannelDispatch::new(mock.clone(), 0);
        let req = fixture_request("a4", "skip");
        let err = dispatch.send(&req, false).await.unwrap_err();
        match err {
            ChannelDispatchError::Disabled(name) => assert_eq!(name, "telegram"),
            other => panic!("expected Disabled, got {other:?}"),
        }
        assert!(mock.sent_messages().is_empty());
    }
}
