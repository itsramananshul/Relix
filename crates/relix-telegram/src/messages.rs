//! Inbound + outbound message types crossing the Bot API
//! boundary. These are the wire types the live `reqwest`-backed
//! client deserializes/serializes; the channel logic above
//! never touches the raw Telegram JSON shape.

use serde::{Deserialize, Serialize};

/// One inbound text message from a Telegram chat. We deliberately
/// model only what the task-native channel needs today: chat +
/// user identifiers, the originating message id (for threading
/// replies), and the text body. Voice, image, inline button
/// payloads, and forwarded-message origin are out of scope for
/// the first slice.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct IncomingMessage {
    /// Telegram's `update_id` — the long-poll pagination cursor.
    pub update_id: i64,
    pub chat_id: i64,
    pub user_id: i64,
    pub message_id: i64,
    /// `username` is optional in Telegram; falls back to empty
    /// for users who haven't set one.
    #[serde(default)]
    pub username: String,
    pub text: String,
}

/// Telegram's `parse_mode` values. `MarkdownV2` is the only
/// supported Markdown flavour today — the original `Markdown`
/// is deprecated. `Html` is included because some channel
/// flows reach for it for richer formatting (links + bold).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ParseMode {
    MarkdownV2,
    Html,
}

impl ParseMode {
    /// The on-wire string Telegram expects in the
    /// `parse_mode` request field.
    pub fn as_wire(&self) -> &'static str {
        match self {
            ParseMode::MarkdownV2 => "MarkdownV2",
            ParseMode::Html => "HTML",
        }
    }
}

/// A reply the channel wants to send. Always threaded under
/// `reply_to_message_id` so Telegram clients render it inline
/// with the originating message.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct OutgoingMessage {
    pub chat_id: i64,
    /// `0` means "do not thread the reply" — Telegram silently
    /// ignores a zero reply id and posts the message as a
    /// top-level chat message.
    pub reply_to_message_id: i64,
    pub text: String,
    /// Optional `parse_mode`. When `None`, the message is sent
    /// as plain text. Approval notifications use `MarkdownV2`;
    /// chat replies default to plain text so an LLM's stray
    /// markdown doesn't trip Telegram's parser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<ParseMode>,
}

impl IncomingMessage {
    /// First-pass sanitisation of the text for inclusion in a
    /// SOL template's `{{MESSAGE}}` substitution. Strips
    /// characters that would break the Coordinator's
    /// pipe-delim wire format or the SOL template parser.
    pub fn sanitise_for_flow(&self) -> String {
        self.text.replace(['|', '\t', '\r'], " ").replace('\n', " ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_round_trips_via_serde() {
        let m = IncomingMessage {
            update_id: 1,
            chat_id: 100,
            user_id: 42,
            message_id: 5,
            username: "alice".into(),
            text: "hello".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: IncomingMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn missing_username_defaults_to_empty() {
        let json = r#"{"update_id":1,"chat_id":100,"user_id":42,"message_id":5,"text":"x"}"#;
        let m: IncomingMessage = serde_json::from_str(json).unwrap();
        assert_eq!(m.username, "");
    }

    #[test]
    fn sanitise_for_flow_strips_pipes_tabs_newlines() {
        let m = IncomingMessage {
            update_id: 1,
            chat_id: 0,
            user_id: 0,
            message_id: 0,
            username: String::new(),
            text: "a|b\tc\nd\re".into(),
        };
        let clean = m.sanitise_for_flow();
        assert!(!clean.contains('|'));
        assert!(!clean.contains('\t'));
        assert!(!clean.contains('\n'));
        assert!(!clean.contains('\r'));
        assert_eq!(clean, "a b c d e");
    }
}
