//! Inbound + outbound message types crossing the Discord Bot API
//! boundary. Snowflake IDs (user_id, channel_id, message_id) are
//! kept as strings end-to-end — they exceed the JavaScript safe-
//! integer range and Discord emits them as strings on the wire.

use serde::{Deserialize, Serialize};

/// One inbound text message from a Discord channel. We model only
/// what the channel needs today: channel + author identifiers,
/// the message id, optional username, and content.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct IncomingMessage {
    pub message_id: String,
    pub channel_id: String,
    pub user_id: String,
    /// Discord exposes both `username` and `global_name`; we capture
    /// `username` (falling back to empty when the wire payload omits
    /// it, which happens for system messages).
    #[serde(default)]
    pub username: String,
    /// `true` when the message author is a bot (including our own
    /// bot). The controller uses this to ignore self-loops without
    /// having to thread the bot's own user_id through every check.
    #[serde(default)]
    pub is_bot: bool,
    /// Message body. May contain mentions, emoji, line breaks.
    pub content: String,
}

/// A reply the channel wants to send. `reply_to_message_id` empty
/// means a top-level message; non-empty produces a Discord reply
/// reference rendered inline by clients.
///
/// `components` carries an optional Discord components array —
/// Action Rows + Buttons / Select Menus per the
/// [`Message Components`](https://discord.com/developers/docs/interactions/message-components)
/// spec. Approval messages stamp this with a single Action Row
/// carrying Approve / Deny buttons; the buttons' `custom_id`
/// encodes the approval id so the bridge's interaction handler
/// can lift the decision in one parse.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct OutgoingMessage {
    /// Discord channel snowflake (kept as string — Discord
    /// emits snowflakes as strings on the wire because they
    /// exceed JS's safe-integer range).
    pub channel_id: String,
    /// Empty == "do not reference". Non-empty produces a reply
    /// reference.
    #[serde(default)]
    pub reply_to_message_id: String,
    /// Plain-text message body.
    pub content: String,
    /// Optional Discord components array. Empty == "do not
    /// attach components". Each element is one component
    /// object (typically an Action Row containing buttons).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<serde_json::Value>,
}

impl IncomingMessage {
    /// First-pass sanitisation for inclusion in a SIMP-016 wire arg.
    /// Strips `|`, `\t`, `\r`, `\n` so the rendered string can't
    /// break the pipe-delimited contract.
    pub fn sanitise_for_flow(&self) -> String {
        self.content
            .replace(['|', '\t', '\r'], " ")
            .replace('\n', " ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_round_trips_via_serde() {
        let m = IncomingMessage {
            message_id: "9000".into(),
            channel_id: "100".into(),
            user_id: "42".into(),
            username: "alice".into(),
            is_bot: false,
            content: "hello".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: IncomingMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn missing_username_defaults_to_empty() {
        let json = r#"{"message_id":"1","channel_id":"2","user_id":"3","content":"x"}"#;
        let m: IncomingMessage = serde_json::from_str(json).unwrap();
        assert_eq!(m.username, "");
        assert!(!m.is_bot);
    }

    #[test]
    fn sanitise_for_flow_strips_pipes_tabs_newlines() {
        let m = IncomingMessage {
            message_id: "1".into(),
            channel_id: "2".into(),
            user_id: "3".into(),
            username: String::new(),
            is_bot: false,
            content: "a|b\tc\nd\re".into(),
        };
        let clean = m.sanitise_for_flow();
        assert_eq!(clean, "a b c d e");
        assert!(!clean.contains('|'));
        assert!(!clean.contains('\n'));
    }
}
