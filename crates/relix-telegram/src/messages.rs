//! Inbound + outbound message types crossing the Bot API
//! boundary. These are the wire types the live `reqwest`-backed
//! client deserializes/serializes; the channel logic above
//! never touches the raw Telegram JSON shape.

use serde::{Deserialize, Serialize};

/// One inbound message from a Telegram chat. Carries either a
/// text body OR a `voice` pointer (in which case `text` is empty
/// and the controller transcribes the voice file before running
/// the chat flow). When the originating update is a
/// `callback_query` (an operator pressed an inline button),
/// `text` holds the button's `callback_data` verbatim and
/// [`callback_query_id`](Self::callback_query_id) is `Some` so
/// the controller can acknowledge the press via
/// `BotApi::answer_callback_query`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct IncomingMessage {
    /// Telegram's `update_id` — the long-poll pagination cursor.
    pub update_id: i64,
    /// Numeric chat id. Set even for callback_query updates —
    /// Telegram includes the originating message in the
    /// callback envelope.
    pub chat_id: i64,
    /// Numeric user id (the operator who pressed the button on
    /// callback_query updates, or the message author for text
    /// messages).
    pub user_id: i64,
    /// Telegram `message_id` of the originating message. For
    /// callback_query updates this points at the message that
    /// carried the inline keyboard.
    pub message_id: i64,
    /// `username` is optional in Telegram; falls back to empty
    /// for users who haven't set one.
    #[serde(default)]
    pub username: String,
    /// Message text. For callback_query updates this is the
    /// button's `callback_data` verbatim so the existing
    /// `/approve <id>` / `/deny <id>` routing keeps working.
    pub text: String,
    /// Telegram `voice.file_id` when the inbound message is a
    /// voice note. `None` for text-only / callback messages.
    /// When set, the controller will (if configured) download
    /// the audio via `BotApi::get_file_bytes` and pass the
    /// bytes to `tool.audio.transcribe`; the resulting text is
    /// what drives the chat flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_file_id: Option<String>,
    /// Set on callback_query updates so the controller can ack
    /// the button press via `BotApi::answer_callback_query`.
    /// `None` for plain text / voice messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_query_id: Option<String>,
}

/// Telegram's `parse_mode` values. `MarkdownV2` is the only
/// supported Markdown flavour today — the original `Markdown`
/// is deprecated. `Html` is included because some channel
/// flows reach for it for richer formatting (links + bold).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ParseMode {
    /// Telegram's `MarkdownV2` parse mode.
    MarkdownV2,
    /// Telegram's `HTML` parse mode (case-flipped on the wire).
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

/// Inline-keyboard layout attached to an outgoing message. The
/// shape mirrors Telegram's wire schema verbatim so
/// `serde_json::to_value(&markup)` produces the exact body
/// Telegram expects:
///
/// ```json
/// { "inline_keyboard": [[ { "text": "...", "callback_data": "..." } ]] }
/// ```
///
/// Outer `Vec` is rows; inner `Vec` is the buttons within a
/// row. Approval messages use a single row with two buttons.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct InlineKeyboardMarkup {
    /// Rows of buttons. Telegram renders each inner `Vec` as
    /// one horizontal row.
    pub inline_keyboard: Vec<Vec<InlineKeyboardButton>>,
}

/// One inline button on an inline-keyboard row. We model only
/// the `callback_data` variant (the only one the approval
/// dispatcher uses). `text` is the button label; `callback_data`
/// is what Telegram echoes back in the resulting
/// `callback_query.data` field when the operator presses it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct InlineKeyboardButton {
    /// Button label rendered to the operator.
    pub text: String,
    /// Opaque string echoed back in `callback_query.data` when
    /// the operator presses the button. Capped at 64 bytes by
    /// Telegram; approval dispatch uses `/approve <id>` /
    /// `/deny <id>`, well within the cap.
    pub callback_data: String,
}

/// A reply the channel wants to send. Always threaded under
/// `reply_to_message_id` so Telegram clients render it inline
/// with the originating message — except when
/// `reply_to_message_id == 0`, which Telegram silently treats
/// as "post as a top-level message". Approval notifications
/// use `0` because there is no inbound message to thread
/// against.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct OutgoingMessage {
    /// Numeric chat id to post into.
    pub chat_id: i64,
    /// `0` means "do not thread the reply" — Telegram silently
    /// ignores a zero reply id and posts the message as a
    /// top-level chat message.
    pub reply_to_message_id: i64,
    /// Message body.
    pub text: String,
    /// Optional `parse_mode`. When `None`, the message is sent
    /// as plain text. Approval notifications use plain text by
    /// default so emoji + readable layout doesn't trip Markdown
    /// parsing; operators can opt into `MarkdownV2` when they
    /// want richer formatting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse_mode: Option<ParseMode>,
    /// Optional inline keyboard attached to the message. When
    /// set, Telegram renders the buttons under the text and
    /// fires a `callback_query` update when the operator
    /// presses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_markup: Option<InlineKeyboardMarkup>,
}

impl IncomingMessage {
    /// First-pass sanitisation of the text for inclusion in a
    /// SOL template's `{{MESSAGE}}` substitution. Strips
    /// characters that would break the Coordinator's
    /// pipe-delim wire format or the SOL template parser.
    pub fn sanitise_for_flow(&self) -> String {
        self.text.replace(['|', '\t', '\r'], " ").replace('\n', " ")
    }

    /// `true` when this incoming message originated from an
    /// inline-button press rather than a text / voice message.
    /// The controller checks this to decide whether to ack via
    /// `BotApi::answer_callback_query`.
    pub fn is_callback_query(&self) -> bool {
        self.callback_query_id.is_some()
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
            voice_file_id: None,
            callback_query_id: None,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: IncomingMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn incoming_callback_round_trips_via_serde() {
        let m = IncomingMessage {
            update_id: 9,
            chat_id: 100,
            user_id: 42,
            message_id: 5,
            username: "alice".into(),
            text: "/approve abc123".into(),
            voice_file_id: None,
            callback_query_id: Some("cb-1".into()),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: IncomingMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
        assert!(back.is_callback_query());
    }

    #[test]
    fn missing_username_defaults_to_empty() {
        let json = r#"{"update_id":1,"chat_id":100,"user_id":42,"message_id":5,"text":"x"}"#;
        let m: IncomingMessage = serde_json::from_str(json).unwrap();
        assert_eq!(m.username, "");
        assert!(m.callback_query_id.is_none());
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
            voice_file_id: None,
            callback_query_id: None,
        };
        let clean = m.sanitise_for_flow();
        assert!(!clean.contains('|'));
        assert!(!clean.contains('\t'));
        assert!(!clean.contains('\n'));
        assert!(!clean.contains('\r'));
        assert_eq!(clean, "a b c d e");
    }

    #[test]
    fn outgoing_serializes_inline_keyboard_in_telegram_shape() {
        let m = OutgoingMessage {
            chat_id: 100,
            reply_to_message_id: 0,
            text: "approve?".into(),
            parse_mode: None,
            reply_markup: Some(InlineKeyboardMarkup {
                inline_keyboard: vec![vec![
                    InlineKeyboardButton {
                        text: "approve".into(),
                        callback_data: "/approve abc".into(),
                    },
                    InlineKeyboardButton {
                        text: "deny".into(),
                        callback_data: "/deny abc".into(),
                    },
                ]],
            }),
        };
        let v = serde_json::to_value(&m).unwrap();
        let keyboard = &v["reply_markup"]["inline_keyboard"];
        assert!(keyboard.is_array());
        let row = &keyboard[0];
        assert_eq!(row[0]["text"], "approve");
        assert_eq!(row[0]["callback_data"], "/approve abc");
        assert_eq!(row[1]["text"], "deny");
        assert_eq!(row[1]["callback_data"], "/deny abc");
    }

    #[test]
    fn outgoing_without_markup_omits_field() {
        let m = OutgoingMessage {
            chat_id: 100,
            reply_to_message_id: 5,
            text: "hi".into(),
            parse_mode: None,
            reply_markup: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(!s.contains("reply_markup"), "got: {s}");
    }

    #[test]
    fn inline_keyboard_round_trips_via_serde() {
        let markup = InlineKeyboardMarkup {
            inline_keyboard: vec![
                vec![InlineKeyboardButton {
                    text: "row1".into(),
                    callback_data: "r1".into(),
                }],
                vec![InlineKeyboardButton {
                    text: "row2".into(),
                    callback_data: "r2".into(),
                }],
            ],
        };
        let s = serde_json::to_string(&markup).unwrap();
        let back: InlineKeyboardMarkup = serde_json::from_str(&s).unwrap();
        assert_eq!(markup, back);
    }
}
