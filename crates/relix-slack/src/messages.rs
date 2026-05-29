//! Inbound + outbound message types crossing the Slack Web API
//! boundary. Slack's user_id, channel_id, team_id, and message
//! timestamp (`ts`) are all strings on the wire and we keep them
//! that way end-to-end.
//!
//! `ts` is Slack's per-message identifier — a unix timestamp with
//! microsecond fraction (e.g. `"1700000000.000200"`). It doubles
//! as the polling cursor (the `oldest` param to
//! `conversations.history`) and the threading anchor (`thread_ts`).

use serde::{Deserialize, Serialize};

/// One inbound text message from a Slack channel. We model only
/// what the channel needs today: channel + author identifiers, the
/// `ts`, optional username, and text.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct IncomingMessage {
    /// Slack message timestamp (`ts`). Doubles as the message id
    /// and the polling cursor.
    pub ts: String,
    pub channel_id: String,
    /// Slack `user` field on the message. Empty for messages
    /// without an authoring user (system messages, some
    /// subtypes).
    #[serde(default)]
    pub user_id: String,
    /// Slack `username` if the message carried one. Slack does
    /// not include the display name on every message, so callers
    /// looking it up by hand should use `users.info`.
    #[serde(default)]
    pub username: String,
    /// `true` when the inbound is a bot message — either the
    /// `subtype == "bot_message"` shape OR `bot_id` is set. The
    /// controller filters these out so it can't trigger itself.
    #[serde(default)]
    pub is_bot: bool,
    /// Slack message body.
    pub text: String,
}

/// A reply the channel wants to send. `thread_ts` empty means a
/// top-level message; non-empty produces a Slack threaded reply.
///
/// `blocks` carries an optional Block Kit layout — when non-
/// empty it is sent through `chat.postMessage` alongside
/// `text` (Slack uses `text` as the fallback / notification
/// preview when blocks render). Approval messages stamp this
/// with a `section` + `actions` block carrying the Approve /
/// Deny buttons.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
pub struct OutgoingMessage {
    /// Slack channel id (`C…` public, `G…` private, `D…` IM).
    pub channel_id: String,
    /// Empty == "do not thread". Non-empty == "post in this
    /// thread."
    #[serde(default)]
    pub thread_ts: String,
    /// Plain-text body. Sent verbatim AND used as the
    /// fallback / notification preview when `blocks` is
    /// non-empty.
    pub text: String,
    /// Optional Block Kit layout. Each element is one block in
    /// Slack's Block Kit shape (`section`, `actions`,
    /// `divider`, etc.). When empty the message is sent as
    /// plain text only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<serde_json::Value>,
}

impl IncomingMessage {
    /// First-pass sanitisation for inclusion in a SIMP-016 wire
    /// arg. Strips `|`, `\t`, `\r`, `\n` so the rendered string
    /// can't break the pipe-delimited contract.
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
            ts: "1700000000.000200".into(),
            channel_id: "C012345".into(),
            user_id: "U67890".into(),
            username: "alice".into(),
            is_bot: false,
            text: "hello".into(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: IncomingMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn missing_username_defaults_to_empty() {
        let json = r#"{"ts":"1.2","channel_id":"C0","user_id":"U0","text":"x"}"#;
        let m: IncomingMessage = serde_json::from_str(json).unwrap();
        assert_eq!(m.username, "");
        assert!(!m.is_bot);
    }

    #[test]
    fn sanitise_for_flow_strips_pipes_tabs_newlines() {
        let m = IncomingMessage {
            ts: "1.2".into(),
            channel_id: "C0".into(),
            user_id: "U0".into(),
            username: String::new(),
            is_bot: false,
            text: "a|b\tc\nd\re".into(),
        };
        let clean = m.sanitise_for_flow();
        assert_eq!(clean, "a b c d e");
    }
}
